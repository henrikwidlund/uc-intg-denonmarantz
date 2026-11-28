# Denon and Marantz integration for Remote Two/3 (Rust)

Integration driver for the [Unfolded Circle](https://www.unfoldedcircle.com/) Remote Two and Remote 3.
It controls Denon and Marantz network receivers with the `denon-marantz` library.

The driver is an alternative to the Python integration that is part of the Remote software. It has its own
driver id (`denonavr_rs`), because the Python integration cannot be stopped or uninstalled. The `config.json`,
the entity names and the command names are the same, so receivers that are set up with the Python integration
work without a new setup. The driver adds what it finds on the receiver to `config.json`: the serial number,
the MAC address, the receiver type, the HTTP read method, `CommApiVers` (the protocol generation), if the
receiver has a tuner, the form and names of the tuner presets, the names of the network favorites, what the
last full connect found (`info`), and the HTTP sources that the receiver does not have (`missing`). The Python
integration ignores them.

The device id in the entity ids is the MAC address of the receiver. Receivers from about 2011 and older do not
give it: they use the serial number. Receivers that are set up with the Python integration keep the serial
number (`id_kind` in `config.json`).

## Status

Preview. Tested with a Denon AVC-A10H. Marantz receivers are only tested with simulated receivers. The
Audyssey values (MultEQ, Dynamic EQ) are not tested on a receiver with Audyssey. HEOS is not tested with
Bluetooth or with zone 2. Please report problems as
[issues](https://github.com/henrikwidlund/uc-intg-denonmarantz/issues).

## Features

- **Media player**: power, volume and mute, input source, sound mode, navigation, menu, options, info, channel
  up and down, play and pause, next and previous. Title, artist, album and cover for network audio (also USB
  and iPod) and HD radio. Receivers with `CommApiVers` 0301 or higher have HEOS. On receivers with HEOS,
  network audio shows what HEOS plays, with position, length, repeat and shuffle, and play, pause, stop, next,
  previous, repeat and shuffle go to HEOS. Volume and mute stay on the receiver: HEOS shares one volume for
  all zones. The Remote must be able to reach the receiver on TCP port 1255 (HEOS) and 8015 (covers from
  AirPlay and USB, seen on an AVC-A10H). Volume up and down use the step of the receiver (0.5 dB). The volume
  step of a Python integration configuration is not used. A volume above the top of the scale that the
  receiver lists is set to the top.
- **Remote**: the simple commands of the Python integration (about 370), `BT_LEVEL_UP`/`DOWN`,
  `TACTILE_CHANNEL_*` (the channel of the tactile transducer, `CVTTR`), commands of older receivers (see
  [Older receivers](#older-receivers)), up to 15 UI pages, button mapping, repeat and button sequences. Text
  of up to 28 characters is sent to the receiver as it is. `volume`, `select_source` and `select_sound_mode`
  are not simple commands: they need a parameter. `stop` is a simple command only on Denon receivers and on
  receivers that may have HEOS.
- **Selects**: sound mode, input source, reference level, dynamic volume, dimmer, eco mode. With telnet also
  monitor output, speaker preset, picture mode, digital codec and, if the receiver has Dirac, the Dirac filter
  (only the slots that have a filter).
  With telnet, the sound mode select offers the modes that the receiver lists for the current input.
- **More entities** (with telnet, some also with HTTP only, see [Requirements](#requirements)): switches for
  tone control, Dynamic EQ, the three trigger outputs and the Bluetooth transmitter; numbers for bass and
  treble; selects for the Audyssey MultEQ mode, the quick select (smart select on Marantz), the tuner band and
  the Bluetooth output; sensors for the active Dirac filter, the Bluetooth level and the sound mode category
  (movie, music, game or pure; only if the receiver lists it). A quick select can
  change the volume. The quick select, its simple commands and its UI page offer only the slots that the
  receiver has.
- **Sensors**: volume, sound mode, input, mute, dimmer and eco mode. With telnet also sleep timer, audio delay,
  audio and video signal, channels, resolution and bandwidth, video output port, HDR, pixel depth, FRL,
  color space (with BT.2020, for example `4:2:2/BT.2020`), picture mode, tuner frequency, all zone stereo and
  digital codec. Some signal sensors also exist with HTTP only.

Entities, simple commands and buttons of a feature that `Deviceinfo.xml` does not list are left out, for
example the reference level, dimmer, eco mode, picture mode or Audyssey entities. Receivers from before about
2016 list no features and get all of them.
With telnet, all other entities exist on every receiver, except the Dirac select, sensor, commands
(`DIRAC_LIVE_FILTER_*`) and buttons, which need a receiver with Dirac, and the entities of [older
receivers](#older-receivers). A receiver without an AM/FM tuner gets no tuner band and tuner preset selects,
no tuner frequency sensor, no tuner commands and buttons, and no channel up and down. The setup option
"Receiver has an AM/FM tuner" decides this. Its default comes from `Deviceinfo.xml`, else, if the receiver is
on, from asking it for its tuner band and frequency over telnet (no answer means no tuner). If neither works,
the setup asks: turn the receiver on before the setup to let the driver find it out. Many newer models have a
tuner only in North America, and some have an analog input named `TUNER` without a tuner, so `Deviceinfo.xml`
often cannot tell. A receiver taken over from the Python integration has the tuner parts until it is
configured again. A value that the receiver does not have shows `--`, for example the Audyssey settings of a
receiver without a feature list and without Audyssey. A setting that the receiver does not accept
in its current state, for example the reference level without Dynamic EQ or the tone control with a Dirac
filter, is ignored. The driver answers OK, so the Remote shows no error, and writes a line to the log that
says why.

## Older receivers

The driver reads `CommApiVers` from `Deviceinfo.xml`. It decides which commands and entities of older receivers
are offered, from the Denon and Marantz protocol specs:

| `CommApiVers` | Receivers | Spec |
|---|---|---|
| 0210 Denon | 2012 and 2013, for example AVR-3313, AVR-X2000, AVR-X4000 | Denon 2011 to 2013 |
| 0250, 0300 Denon | 2014 and 2015, for example AVR-X1100W, AVR-X2100W, AVR-X4100W | Denon 2014 and 2015 |
| 0210 Marantz | 2013, for example SR5008, NR1604 | Marantz 2013 |
| 0301 and higher | 2016 and later, with HEOS | current specs |

A receiver with another value, without a value, or not read yet gets all of them. The brand comes from what the
receiver reports. If it reports none, or before it is read, the brand setting is used. A receiver ignores a command
that it does not have, so a button can do nothing on some models.

- Simple commands: `PRO_LOGIC_MODE_*`, `SURROUND_BACK_MODE_*`, `FRONT_HEIGHT_*`, `DYNAMIC_COMPRESSION_*`,
  `AUTO_FLAG_DETECT_*`, `DIALOG_LEVEL_ADJUST_*`, `DIALOG_LEVEL_UP`/`DOWN`, `AUDYSSEY_DSX_*`,
  `STAGE_WIDTH_UP`/`DOWN`, `STAGE_HEIGHT_UP`/`DOWN`, `HEIGHT_GAIN_*`, `EFFECT_SPEAKER_SELECTION_OFF`,
  `INSTA_PREVUE_*`, `SOURCE_MENU_*`, `INPUT_MODE_7_1_IN`, `LFE_7_1_IN_*`, and `NETWORK_*` (repeat, random,
  next, previous, page) for network audio on receivers without HEOS. Toggles need telnet.
- Sensors (telnet): Pro Logic mode, surround back mode, front height, dynamic compression, auto flag detect,
  Dialog Level Adjust and dialog level, Audyssey DSX, stage width and height, height gain, InstaPrevue. The
  driver asks for these values after each connect.
- Selects (telnet): network favorites on receivers without HEOS (36 on Denon 0250 and 0300, else 56), and the
  tuner presets on receivers that may have a tuner. They show the names that the receiver reports, or the
  frequency of a tuner preset without a name. The driver asks for the names once per connection, the first
  time the receiver is on and the tuner or network audio plays, and keeps them in `config.json`. Until then the
  selects show `Preset n` and `Favorite n`. On older receivers a tuner preset is selected only after the
  receiver has shown if its presets are `01` to `56` or `A1` to `G8`; the driver saves this. Receivers with
  0301 and higher use `01` to `56`. On some older US models the receiver does not report the tuner frequency
  and preset.

Commands with a number are sent as raw text with the remote entity:

| Text | What it does |
|---|---|
| `NSB00` to `NSB55` | Play network favorite 1 to 56, 1 to 36 on Denon 0250 and 0300 (receivers without HEOS) |
| `NSC00` to `NSC55` | Save what plays as network favorite 1 to 56, 1 to 36 on Denon 0250 and 0300 |
| `TFAN` + 6 digits | Tune the tuner (FM: `TFAN010110` = 101.10 MHz, AM: `TFAN105000` = 1050 kHz) |
| `TPANMEM01` to `TPANMEM56` | Save the tuned station as tuner preset 1 to 56 |
| `TFHD` + 6 digits | Tune the HD Radio tuner, in the same way (North America models) |

`NSC` replaces the favorite. After `NSC` and `TPANMEM` the driver reads the names again. The 6 digits of `TFAN` and
`TFHD` are hundredths of MHz for FM and hundredths of kHz for AM. Receivers with lettered tuner presets use `TPANMEMA1`
to `TPANMEMG8`.

## Compared to the Python integration

- The receiver is read in a fraction of a second after the Remote wakes up. The values of telnet and the web
  pages follow in the background.
- With telnet, values come from the receiver as events. Receivers from about 2016 and later are also asked for
  the signal info every 5 seconds while they are on, because they do not report every change. The setup option
  for the signal check sets this time; 0 turns the check off. While telnet is down, the signal is read over HTTP
  at this time, rounded up to a step of 10 seconds. Older receivers have no such check, and the setup
  does not show the option for them.
- Entities keep their state while the Remote sleeps.
- Only receivers with entities in use are connected. The Python integration connects all of them.

## Network and wake-up

- The driver connects only to receivers that are in use: a Remote that is awake subscribed at least one of
  their entities. A receiver that no awake Remote uses is disconnected. A command to a receiver that is not in
  use connects it (the driver logs a warning), and the next change of the subscriptions or of a Remote
  disconnects it again. When a Remote goes to standby, sends `disconnect` or ends, the driver also stops the
  connect tries of such a receiver.
- Only the Remote changes its subscriptions. A subscription without entity ids, or with an empty list, means all
  entities, also entities that come later. An unsubscribe of some entities ends this: entities that come later
  are not subscribed. An unsubscribe without entity ids means all entities; one with an empty list changes
  nothing.
- The driver connects with the address and the values (`info`) of the last full connect. This sends no request:
  the start checks them. If the values changed, the driver saves them; if the type, the port or the read method
  changed, it connects again. If the start fails, or another receiver answers, the driver looks the receiver up:
  the last address first, then the IP addresses of the host name (from the setup, or an old address), then it
  searches the network for its MAC address or serial number. It saves the new address.
- The setup accepts an IP address or a host name. For a host name, the driver saves the name (`host` in
  `config.json`) and its IP address (`address`). It connects to the IP address, so a normal connect does not
  look up the name. It uses the first IP address where the receiver answers. The setup waits at most 3 seconds
  for the name, and at most 10 seconds for the name and all its IP addresses together. When you add a receiver
  and the setup cannot look up the name, it connects with the name; if that fails, nothing is saved. When you
  add a receiver and no address answers in this time, nothing is saved. When you change the address of a
  configured receiver and the receiver answers at none in this time, the setup saves the first IP address where
  no other receiver answers. It saves the name as the address if other receivers answer at all IP addresses or
  if it cannot look up the name. If `address` in `config.json` is a host name and
  `host` is not set, the driver moves the name to `host` when it saves a new address.
- After the Remote wakes up, the network of the Remote can be down for a short time. The driver tries again
  for up to 3 seconds. A connection to the receiver that does not open in 0.25 seconds is tried again. A start or
  lookup of the receiver that takes more than 3 times as long as the fastest of the last 5 starts (at least 0.2
  seconds) is started again, at most 4 times. There is no such limit for the first connect, or if that start took
  more than about 0.7 seconds. If telnet or HEOS cannot connect after wake-up, they are tried again every 0.05
  seconds for 3 seconds, then less often (HEOS only if the receiver has it).
- When the telnet connection gets no data for 10 seconds, the driver asks the receiver for its power state. When
  it gets no data for 20 seconds, the connection counts as lost and the driver connects again.
- A command that comes while the first connect runs waits until it ends, within the 9 seconds below. It is not
  sent if the Remote goes to standby or the setup connects again during the wait. Commands that do not need the
  state of the receiver (power, volume, mute, cursor keys, back, info, raw text, and next, previous and stop on
  receivers without HEOS) are sent when the receiver is known, before the start ends. Channel up and down read
  the source: on the HD Radio source they go to the HD Radio presets.
  Commands keep their order: a command after one that waits also waits. A command whose connection cannot be
  opened is tried again for up to 3 seconds; a command sent before the start ends stops when the Remote goes to
  standby or wakes up again. If the receiver is not connected and no connect runs, a command fails at once.
- A command that was surely not sent is sent once more: there was no receiver after the wait, or the telnet
  connection was not open. It waits for a connect that runs, and the next try goes over HTTP if telnet is still
  down. It is not sent again after standby, a disconnect or a setup change that connects again, or for a
  command that goes to HEOS (HEOS may have received it). When less than 1 second is left of the 9 seconds
  below, or the connect still runs after the wait, the answer is a timeout. Remote commands with more than one
  step are not sent again.
- A command is answered at most 9 seconds after it came (the Remote waits 10 seconds). If it cannot be sent before
  then, the answer is a timeout and nothing is sent. A send that still runs then gets a timeout answer and ends later.

## Requirements

- A Denon or Marantz network receiver.
- The receiver and the Remote are on the same network, or you enter the address of the receiver in the setup.
- Network control is on in the receiver, also in standby. A fixed IP address (reservation in the router)
  or a host name is recommended.
- Some old receivers allow only one telnet connection. If telnet does not work for you, use the connection
  mode _Use HTTP connection_. In this mode the driver reads the receiver every 10 seconds and after each
  command. All receivers have the media player and the remote. The selects, sensors, switches and numbers
  come only if the library reports that the driver reads their value over HTTP. Until the
  receiver is read for the first time, there are 6 selects (sound mode, input source, reference level,
  dynamic volume, dimmer, eco mode) and 7 sensors (volume in dB, absolute volume, sound mode, input source,
  mute, dimmer, eco mode). Receivers with the newer HTTP interface can also have the MultEQ select, the tone
  control and Dynamic EQ switches, the bass and treble numbers, the monitor output select and sensor, and
  the sensors for the audio signal, sampling rate, HDMI video in and out, and the input and output channels.
  Receivers with the older HTTP interface have no dimmer, reference level and dynamic volume. Receivers that
  list their values but not the Audyssey values have no reference level and dynamic volume. The sensors of
  the signal web pages come too, for example HDR and color space, if the receiver has the pages. The signal
  info is read at the time of the signal check option, rounded up to a step of 10 seconds. With 0, only
  `AppCommand0300` is read every 10 seconds, on receivers that use it (Audyssey, HDMI output, audio signal,
  sampling rate, HDMI signal and channels); the sensors of the signal web pages are not shown. Older receivers
  do not show the option: they use 5, or the stored value. The sound mode
  select offers the sound mode groups, for example `MOVIE`. While the receiver is busy, mostly in the first
  seconds after a connect, a changed dimmer or eco mode can take up to 10 seconds to show.

Changing the connection mode does not remove entities. If you add entities while
using telnet and then change to HTTP only, the entities that need telnet stay in the integration as unavailable.
Remove them in the web configurator of the Remote.

## Install on the Remote

1. Download `uc-intg-denonmarantz-rs-<version>-aarch64.tar.gz` from the
   [releases](https://github.com/henrikwidlund/uc-intg-denonmarantz/releases).
2. Web configurator: Integrations > Add new > Install custom. Choose the file. Remove the receiver from the
   Python integration, so that only one integration is connected to it.
3. To keep the old setup, copy the `config.json` of the Python integration to the configuration folder of
   the driver. Otherwise start the setup. Leave the address empty to search the network. If the setup fails,
   run it again. The receiver must send its description.
   With a copied `config.json`, the driver reads `CommApiVers` at the first connect. If the media player entity
   was added with the Python integration, add it again to get the position, length, repeat and shuffle.

Run the setup again to add, remove or configure a receiver, make a backup, or restore one.

After an update, add the remote entity again to get new UI pages and simple commands. The Remote keeps the
list that it got when the entity was added.

## Run on a computer

The driver can also run on a computer as an external integration. The Remote finds it with mDNS.

```shell
cargo run --release
```

| Variable                   | Default           | Description                                                     |
|----------------------------|-------------------|-----------------------------------------------------------------|
| `UC_CONFIG_HOME`           | working directory | Folder for the configuration file.                              |
| `UC_INTEGRATION_INTERFACE` | `0.0.0.0`         | Network interface to listen on.                                 |
| `UC_INTEGRATION_HTTP_PORT` | `9090`            | WebSocket port.                                                 |
| `UC_DISABLE_MDNS_PUBLISH`  | `false`           | Do not announce the driver with mDNS.                           |
| `UC_SETUP_TIMEOUT`         | `3600`            | Setup timeout in seconds. `0` disables it.                      |
| `UC_API_MSG_TRACING`       | off               | Log text messages: `all`, `in` or `out`. No backup data.        |
| `RUST_LOG`                 | `info`            | Log level, for example `info,denon_marantz=debug`.              |

The lines of `UC_API_MSG_TRACING` are debug lines. They show only with the debug log: the setup option or a
`RUST_LOG` with `uc_intg_denonmarantz=debug`.

## Run with Docker

Tags publish the image `henrikwidlund/uc-intg-denonmarantz` on Docker Hub. The configuration is in `/data`.
On Linux, use the host network so that the Remote finds the driver with mDNS.

```shell
docker run -d --network host -v uc-intg-denonmarantz:/data henrikwidlund/uc-intg-denonmarantz
```

The port is 9090. Set `UC_INTEGRATION_HTTP_PORT` to change it.

## Debug log

For an error report, turn on the debug log: tick _Debug log_ in the setup and save. The driver and the
library then write more lines to the log. It stays on after a restart. `RUST_LOG` overrides this setting.

Log line layout: `[<UTC time> <level> <target>] <message> <fields> <spans>`. The spans name the receiver connection
of the library, for example `avr{id=1 host=192.168.1.20}` or `heos{id=1 host=192.168.1.20}`. On Linux, when the
output goes to the systemd journal (`JOURNAL_STREAM` names the stderr of the driver), each log entry starts with a
priority prefix such as `<7>`. The journal then gets the level of each entry, and `journalctl -p warning` shows only
warnings and errors. The second and later lines of a message with more than one line, and the output of a panic, get
the default priority of the journal.

## Backup and restore

Run the setup again and choose _Backup configuration_. Copy the text and keep it in a safe place. To restore,
choose _Restore configuration from backup_ and paste the text. It replaces all configured receivers. The
driver never writes the backup data to the log.

## Build

Needs Rust 1.100 or later.

```shell
cargo build --release
```

The release build for the Remote uses the [UCR2 toolchain](https://github.com/unfoldedcircle/ucr2-toolchain).
See `.github/workflows/build.yml`. A `v*` tag creates a GitHub release with the archive. A tag with `-`, for
example `v1.2.0-beta.1`, creates a pre-release.

The version comes from the git tag, and the release date is the build date. A build without git data, for
example in Docker, takes them from `UC_VERSION` and `UC_RELEASE_DATE` (`YYYY-MM-DD`).

### Development

```shell
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features dhat-heap -- -D warnings
cargo test --locked
cargo deny check
```

The allocation tests use dhat. They count the allocations of all threads, so they run alone:

```shell
cargo test --locked --features dhat-heap dhat_ -- --ignored --test-threads=1
```

Each test fails when the mean allocations of one run, rounded up, are above its limit. The limit is the count
measured with the minimum Rust version (`rust-version` in `Cargo.toml`), which CI uses. Add `--nocapture` to
see the measured allocations and bytes. A new test is named `dhat_…`, is marked `#[ignore]` and goes in
`src/entities/alloc_tests.rs`, which builds only with the `dhat-heap` feature.

`tools/manual_test.sh` runs the tests that need a real Remote, one step at a time: the Remote finds the driver
with mDNS, the setup pages, the entities on the Remote, the pages and buttons of the remote entity (also a held
button), adding and removing a second receiver, and the sleep and wake of the Remote. It saves the results to
`test-results/manual-test-results-<time>.md`. Run `tools/remote-sim` (below) first: it tests everything else.

`tools/remote-sim` is a program that acts as the Remote and runs all other tests with a real receiver. Some checks ask
you to do something (see below). It opens a second connection to the receiver and checks that every select and sensor
shows what the receiver has, after each change. The source select and sensor are not compared when two sources of the
receiver have the same name (a source without a name counts with its code). It also sends the simple commands that the
driver offers on the remote entity, but not the commands only for Marantz receivers, commands with `VOLUME` in the name
(also Dynamic Volume), commands with `RESET` as a whole part of the name, the tuner preset, band, frequency and mode
commands (they cannot be set back), Quick Select, Smart Select, zone favorites, memory commands, the network restart,
the triggers, the remote lock and the LFE level of the 7.1 input. The speaker and Auro-Matic 3D presets are sent and set
back. It starts the driver without mDNS, so it needs no root rights. The checks switch the receiver on and off and
change source, sound mode and some settings, and restore them, also when a check fails, unless you choose at a question
to leave the source as it is. The `mute` check runs only if the mute state is known, the receiver is not muted, its
volume is at or below -40 dB and the driver shows the same volume as the receiver. An unmute comes only after the volume
is at or below the volume before the change and at or below -40 dB (the volume of the receiver, or of the driver without
the second connection). At the end, `restore_power` sets the power and the mute state back to the start and, with the
second connection, lowers the volume to the start volume (not above -40 dB) if it is higher. If the receiver does not
answer the volume query, it starts new queries for up to 30 s, and each query waits up to 10 s for its answer. While the
simple commands are sent, the sound can stop for some seconds, for example when HDMI audio goes to the TV. The settings
that they change and the source are saved in `remote-sim-restore.txt` in the repository folder first. A source that may
not be selected is saved only as a line that is never sent. If the file has such a line and also a source line that
would be sent, nothing is set back and the file stays. If the source is not read, the check is skipped. If a run stops
before they are set back, the next run sets them back at its start. If the file has no source line, nothing is set back
and the file stays: the settings could go to another source. The same holds while the main zone is not on. A sleep timer
that counts down is not saved: an old value would set a longer time. `settings_restore` is skipped while the sleep timer
counts down. Lines of the file that do not set a saved setting are never sent, and neither is an All Zone Stereo line
other than `MNZST OFF` (All Zone Stereo on can switch on other zones) or a second answer to a query that has only one
answer line (the channel levels have one line for each channel, and the Bluetooth output and the tactile transducer
level answer with an on or off line and a value line, each kept once): the note on the terminal names them. A left-out
line that sets a value counts as the answer to its query, so a later line of the same kind is left out too. The volume
is read before this set back, and after it a volume above that volume or above -40 dB is lowered to it (it is never
raised), and the note says so. If a value changes while the settings are read (two different answers to one query, for
example `PSBAS 50` and `PSBAS 52`), the read is refused: the check is skipped, or the set back fails, and the text names
the lines. At the end of `simple_commands` and `settings_restore`, the set back is watched (see the volume watch below):
a rise is set back and the check fails. A volume above -40 dB after the set back is lowered to -40 dB and the check
fails. A volume that is not known fails the check too. In both cases `simple_commands` asks you to set the source back
if it changed. `settings_restore` is skipped if no volume is known or the volume is above -40 dB. If a volume read or a
lowering failed while the simple commands were sent or set back, the set back selects no source: if the source changed,
it asks you to set it back. The compares after the source commands and the compare at the end are watched (see the
volume watch below): a rise is set back and the check fails; if a volume read or a lowering fails there, no more
commands are sent. A sound mode that the receiver does not take is only a note and does not keep the file: the input
signal can limit the sound modes. It sets the source back only if the main zone is on, exactly one source was saved, and
both the saved source and the current source may be selected (see `--avoid-sources`). Some settings are kept for each
source, so if the source cannot be set back and is not the saved source already, nothing is set back and the file stays.
This is also true at the end of a check, and when the source must be set back while the main zone is off. If the saved
source was the source at the start of this set back, the source rule was not checked: if the source changes before or
while the settings are sent (each read of the receiver checks it), nothing more is sent and the file stays. If the
receiver does not take the saved source or the saved speaker preset, nothing more is set back and the file stays: the
other settings could change the settings of another source or preset. This is also true if the receiver loses the saved
source during the set back. If the source changed and the set back of `simple_commands` stopped early, it asks you to
set the source back. `settings_restore` then sets only the sleep timer back and removes the file: it changes no other
setting. The note says why the set back stopped. While the file is still there, the checks that change these settings
are skipped. A source change stops HEOS and AirPlay, and they do not start again. The checks never set the volume above
-40 dB. A power on can start with a higher volume of the receiver: if it is above -40 dB, it is lowered to -40 dB at
once. When `signal` or the Bluetooth part of `media_info` finds a volume above -40 dB, it lowers it and fails. `heos`
and next and previous of `media_keys` do the same once HEOS plays after you started it; when HEOS already plays at the
start, `heos` and next and previous of `media_keys` are skipped on a volume above -40 dB. If only the driver shows that
volume and the receiver is not above -40 dB, it first waits up to 5 s for the driver to show -40 dB or less: then it
does not fail. This wait is watched too. If the second connection cannot read the volume, a check that makes sound or
changes the source fails. If a source must be set back then, it asks you to do it. `ws_keepalive` and `ws_close` use
another WebSocket connection to the driver: `ws_keepalive` sends a ping every second for 5 s and needs each pong within
1 s. `ws_close` closes with code 4001 and reason `bye`, needs the same code and reason back, and the driver log must
show within 3 s that the session ended.

Volume watch: a check that makes sound reads the receiver volume about every second while it waits and sound can play:
while a question asks you to play something or to set the source back, while it waits for the signal, the source,
Bluetooth or HEOS after your answer, while HEOS plays in `heos` and next and previous of `media_keys` (also when HEOS
already plays at the start) and while HEOS is paused at their end, while a source that the check selected plays or is
set back, while `http_mode` changes the sound mode and the bass and sets them back, while `sound_mode`, `select_sweep`
and `extra_entities` change a setting and set it back, while `simple_commands` and `settings_restore` set the settings
back, and while `simple_commands` compares the entities after its source commands and at the end. If the volume goes
above the volume at the start, the wait ends, the volume is set back and the check (or the part) fails. This is also
true when the volume goes up at the same time as your answer. Right after each wait ends, also after s + Enter, the end
of the input, an error or a time-out, the volume is read once more and set back the same way. If a volume read or a
lowering fails, the check fails, and for the rest of the check it asks no more sound questions, sends no sound command
and selects no source: if the source changed, it asks you to set it back. A rise is always in the result, also when the
check stops with another error. The volume at the start is the receiver volume: a check that makes sound is skipped
without the second connection. When HEOS already plays at the start, the check only reads the volume at the start.

More checks:

- `values` compares the selects, sensors, switches and numbers with the second connection, also the HDR input and output
  sensors. An HDR sensor without a value on the second connection is compared only on a receiver with HEOS, which gives
  the signal on telnet; the other values that come from a web page are not compared without a value.
- `signal` (after `values`) asks you to play an HDR10 or Dolby Vision source, then an SDR source. Each question lists
  the sources in `--avoid-sources`: do not use them. With `--avoid-sources`, the check is skipped if the source names
  page cannot be read, or if the input levels are above 0 dB or not known.
  - Signal: it waits up to 20 s until the second connection has the signal, else it asks again. HDR means BT.2020 or an
    HDR format on the input or the output. SDR means a known color space without BT.2020 and without an HDR format. Then
    it compares the color space, HDR and pixel depth sensors with the second connection. A sensor without a value on the
    second connection is not compared if the value comes from a web page: the page can have failed. The HDR sensors are
    such sensors only on a receiver without HEOS, which gives no signal on telnet: the SDR note then says that HDR was
    not compared.
  - Time: the driver must show the change within the signal check time + 5 s. In HTTP mode this time is rounded up to a
    multiple of 10 s. Without a signal check (a time of 0, or telnet mode on a receiver without HEOS), a late change
    does not fail.
  - Skip: the check is skipped without a terminal, the second connection, signal sensors or a source shown by the
    driver, if no volume is known or the higher of the receiver and the driver volume is above -40 dB, and if you skip
    both the HDR and the SDR question.
  - Volume: the volume at the start is read before the source names page. If the receiver volume is above it after that
    page and `config.json` are read, it is set back and the check fails before the first question. The questions and the
    waits for the signal on the second connection and in the driver are watched (see the volume watch above): a rise is
    set back and that signal kind fails. After each question, and once more at the end, a volume above -40 dB is lowered
    to -40 dB and the check fails. If a volume read or a lowering fails, the check fails and asks no more questions; it
    asks you to set the source back. At the end a receiver volume above the volume at the start is set back, and the
    check fails.
  - Set back: at the end the start source is set back (see the source set back below).
- `heos` compares the media player with a second HEOS connection, and pauses and plays once if the item can pause. If
  HEOS plays nothing on the main zone, it asks you to start HEOS Music. The question is not asked, and the check is
  skipped, if one of these is true: no terminal, no `--avoid-sources`, no second connection, no volume is known, the
  higher of the receiver and the driver volume is above -40 dB, the source names page cannot be read, the input levels
  are above 0 dB or not known, the start source cannot be set back, or the second connection does not know the start
  source after one `SI?` query. If the receiver was off, the check switched it on and then skips before the question or
  because you do not say that HEOS plays, the receiver is put back to standby. You select the HEOS source (`NET`), so
  `--avoid-sources` does not apply to it. Once HEOS plays, a volume above -40 dB is lowered to -40 dB and the check
  fails; if that lowering or the volume read fails, it asks you to set the source back if it changed. Repeat and shuffle
  are compared with the HEOS state read just before. The question, the wait for HEOS and the compare while HEOS plays
  are watched (see the volume watch above). When HEOS already plays at the start, the compare is also watched, and right
  after it a receiver volume above the volume at the start is set back (the check then fails). The check is then skipped
  without the second connection or if the volume is above -40 dB or not known. After this question the check always ends
  like this, also after a failure: it reads the events for a moment (also watched), pauses HEOS if it plays (or stops it
  if the item cannot pause, also watched), right after the pause sets a receiver volume above the volume at the start
  back (the check then fails), and sets the start source back (see the source set back below). If a volume read or a
  lowering failed, the check asks you to set the source back.
- `media_info` (after `heos`) is skipped without `--avoid-sources` or the second connection. It is also skipped if the
  driver shows no tuner entities and there is no terminal. It fails if a part fails and is skipped only if both parts
  are skipped. The start source is read once, before the parts. The parts:
  - Tuner: it selects the `TUNER` source (not HD Radio) and checks the title (the title from the receiver, else the
    frequency text) and the artist (the artist from the receiver, else the band). While the tuner plays and while the
    source is set back, the volume is watched (see the volume watch above). Then it sets a receiver volume above the
    volume at the start back (the part then fails, also if the volume is not known), and sets the start source back. If
    that fails, it asks you to set the source back, and the part fails. If a volume read or a lowering failed, the check
    does not select the start source: it asks you to set it back, and the Bluetooth part is skipped. If that happens
    right after a set back that worked, it does not ask you. The part runs only if the driver shows the tuner entities
    and the frequency sensor (the driver shows it only in telnet mode), the `TUNER` source and the start source may be
    selected and are in the source list, and a volume is known and the higher of the receiver and the driver volume is
    at or below -40 dB. If the tuner is the start source, no source is selected. The volume at the start is read before
    the source names page. If the receiver volume is above it after that read, it is set back and the part fails before
    the tuner is selected; if a volume read or a lowering fails there, the Bluetooth part is skipped.
  - Bluetooth: it asks you to play music from a phone over Bluetooth, then shows the media info and asks if it is right.
    The part runs only with a terminal, input levels at or below 0 dB and a volume of the receiver and the driver at or
    below -40 dB. It is skipped if the start source is not back after the tuner part, or if a volume read or a lowering
    failed in the tuner part. A rise that was set back does not skip it by itself. You select the `BT` source, so
    `--avoid-sources` does not apply to it. If the receiver volume is above the volume at the start after the source
    names page is read, it is set back and the part fails before the play prompt. If the volume is above -40 dB when
    Bluetooth starts, it is lowered to -40 dB and the part fails. The play prompt, the wait for the Bluetooth source,
    the wait for the media info, the question, the moment at the end and the stop prompt are watched (see the volume
    watch above). A stop prompt with a rise while or right after it is asked again, at most 3 times in total; the report
    has each rise. Before the question, at the end and after the stop prompt, a receiver volume above the receiver
    volume at the start is set back, and the part fails. At the end this comes first. Then the start source is set back
    (see the source set back below; if a volume read or a lowering failed, the check asks you to do it), and it asks you
    to stop the music on the phone.
- `media_keys` (after `remote_commands`) sends the navigation and playback keys of the media player. It sends no volume
  or source command, except the lowering after a power on, the lowering of a volume above the volume at the start while
  HEOS plays (see next and previous), and when it asks you to start HEOS: then a volume above -40 dB or above the volume
  at the start is lowered and the start source is set back. The parts:
  - Menu: with the second connection and a receiver that has the menu query, it opens the setup menu, sends the cursor
    keys down, up and right, enter and back while the menu is open, and closes it. Left is not sent: it closes the menu.
    The menu is always closed at the end. It is skipped if the menu is open at the start.
  - Info and option: with the same needs, it sends info, option and back while the setup menu is closed.
  - Channel: only on the analog tuner on a preset, at or below -40 dB: channel up, then down, and the preset and
    frequency must come back. It is skipped if the preset and the frequency do not change after channel up: the tuner
    may have only one preset.
  - Next and previous: only while HEOS plays a queue item on `NET` with at least 30 s left, shuffle off and a next item,
    at or below -40 dB: next, then previous, and the item must come back. Previous is not sent, and the part is skipped,
    if the next item has less than 30 s left: the HEOS queue then stays on the next item. If the position of the next
    item is not known, previous is sent. If HEOS does not play, it asks you to start HEOS Music, with the same rules as
    the `heos` check (see above); s + Enter then skips only next and previous. Then the start item is what HEOS plays
    once the source is `NET`; the part is skipped if HEOS does not play then. If the start item plays again before
    previous (for example after previous in the HEOS app), the part passes and previous is not sent if the driver shows
    the next title and then the start title again (else the part fails). If HEOS plays another item instead of the next
    item before previous, the part is skipped; if HEOS paused or stopped, the part fails. The receiver is not put back
    to standby here: `restore_power` does it at the end. If the driver does not show the HEOS title within 8 s (4 s when
    HEOS already played at the start), the part is skipped. A wrong driver title after next fails the part also when
    previous is not sent. The keys are watched (see the volume watch above); if the watch ends them, the report says
    that next and previous may not be done and that the HEOS item may not be set back. When HEOS already plays at the
    start, the part is skipped without the second connection or if no volume is known, and right after the keys a
    receiver volume above the volume at the start is set back (the part then fails). After this question the part always
    ends like the `heos` check: it pauses or stops HEOS, sets a receiver volume above the volume at the start back (the
    part then fails) and sets the start source back.
- `source` selects another source (see `--avoid-sources`), checks that a source without media shows its name as the
  title, and sets the start source back. The checks that work in standby run first (`--avoid-sources`, the second
  connection, two sources in the driver list), so the receiver is not switched on only to skip. It is skipped if the
  volume is above -40 dB or not known. While the new source plays and while the start source is set back, the volume is
  watched (see the volume watch above). If a volume read or a lowering fails, or the set back fails, the check asks you
  to set the source back. If a volume read or a lowering fails right after a set back that worked, it does not ask you.
  If the receiver was off and the check skips after it switched it on, the receiver is put back to standby.
- `http_mode` switches the driver to HTTP mode and back to telnet. In HTTP mode it sets the signal check time to 0 and
  checks that the HDR, pixel depth, FRL and color space sensors are removed, then sets it to 5 and checks that they are
  back. It lowers the bass by one step and sets it back, only if tone control is on and the Dirac filter is off, and not
  if the volume is above -40 dB or not known (the change is watched, see the volume watch). A bass value with decimals
  is rounded to whole dB. The report shows the MultEQ and Dynamic EQ values. It changes the dimmer (if the driver offers
  it over HTTP) and the sound mode (if the driver showed one with telnet). It selects another source (see
  `--avoid-sources`), but not if the volume is above -40 dB or not known. The source change and its set back are watched
  like in `source` (see the volume watch above). It sets the signal check time, the source, the dimmer and the sound
  mode back. If the set back of the source in HTTP mode fails, or a volume read or a lowering fails while the new source
  plays, it asks you at once to set the source back, before the switch back to telnet. If a volume read or a lowering
  fails while the source is set back and the set back works, it does not ask you, and the source is not selected again
  after the switch back to telnet. If the check cannot set the source back after the switch back to telnet, it asks you
  too. The sound mode change and its set back are watched (see the volume watch above) with the volume at the start of
  the source change, or else with the volume read before the sound mode change: a rise is set back and the check fails.
  The sound mode is not changed if that volume is above -40 dB or not known. If a rise in the sound mode change was not
  lowered or a volume read failed, the check selects no source after the switch back to telnet: if the source is not the
  start source, it asks you to set it back. If you leave the source as it is, the check fails. If the old sound mode
  does not come back, it is only a note if the driver shows a mode and the second connection shows that the receiver is
  no longer in the mode set in HTTP mode: the input signal can change, for example with a source change, and the
  receiver then picks a mode that fits the signal. It fails without the mode of the second connection, if the second
  connection did not show a new mode within 5 s after the change, and if the receiver is back in its mode from before
  the change. If the switch back to telnet fails, the signal check time and the sound mode are not set back, and the
  check fails.
- `address_lookup` (after `config_file`) writes the documentation address `192.0.2.1`, where nothing answers, as the
  stored address in `config.json` and starts the driver again. The driver must find the receiver and store its address
  again. It writes the saved `config.json` back at the end. The parts:
  - Host name: with `--host-name`, the host name is stored too, and the driver must use it. The part is skipped without
    `--host-name`, or if the name does not resolve to the receiver address on this computer.
  - Search: without a host name, the driver must find the receiver by its MAC address or serial number within 20 s.
    While the driver still connects, the dimmer is set to the option it already has: the command must wait for the
    connect and work.
- `configure_options` (after `http_mode`) changes "Show all sources", sets it back and checks the source list without a
  restart of the driver.
- `log` fails on a panic or an ERROR line in the driver log (except the line of the broken file check). It shows the
  WARN lines and asks once if all of them are expected: n + Enter fails the check.

Source set back after a question (`signal`, `heos`, Bluetooth part of `media_info`, next and previous of `media_keys`):
if the source changed, the check selects the start source again only if the volume is known, at or below -40 dB and not
above the volume at the start, and the start source and the current source may be selected (see `--avoid-sources`), and
no volume read or lowering failed. The read of the source names for this rule is watched too (see the volume watch
above). If the volume goes above the volume at the start while or right after the read of the source names, the check
does not select it. Else it asks you to set it back and waits up to 15 s for the driver to show it, then up to 15 s more
for the second connection. The question and this wait are watched (see the volume watch above). After your answer, also
after s + Enter or the end of the input, a receiver volume above the volume at the start is set back, and the check (or
the part) fails, also if the volume is not known. If you leave the source as it is, the check (or the part) is skipped
when nothing failed. For this set back, the volume at the start is the receiver volume: the driver can show an old
volume after a power on. The check is skipped if the driver or the receiver volume is above -40 dB, or without the
second connection. When any check selects the start source again, it waits for the driver to show it, and then for the
second connection to have it too.

The questions are asked on the terminal. Each question says what each key does. What s + Enter does is different for
each question: for example it skips the check, skips a part of it, leaves the source as it is or only adds a note. The
questions and the answers go to the report line of the check. Without a terminal, no question is asked: the check or the
part of it that needs the answer is skipped, and `log` does not check the WARN lines. If a check cannot set the source
back itself and needs to ask you, it fails without a terminal: the source stays changed. A run in the background (for
example with `&`) whose input is the terminal is paused at once, until you bring it back with `fg`. Start such a run
with `< /dev/null`: then no question is asked.

```shell
cargo run --release --manifest-path tools/remote-sim/Cargo.toml -- <receiver address>
```

Before it starts the driver, the tool builds it with `cargo build --release --locked` in the repository folder. It
starts the binary that cargo names, so the target folder of cargo is used, for example from `CARGO_TARGET_DIR`. If the
build fails, the tool stops before it connects to the receiver.

Options:

- `--only a,b` and `--skip a,b` select checks by name. Both can be given more than once. Unknown step names are refused.
  `--only` also runs `setup`, `entities` and `states`, because the other checks need them, and `--skip` refuses them.
  `restore_power` and `log` always run at the end, also with `--only` and `--skip`. If `metadata`, `setup`, `entities`
  or `states` fails, only `restore_power` and `log` run after it.
- `--kinds a,b` limits the select sweep to these selects, for example `dirac_filter`.
- `--avoid-sources a,b`: the checks never select a source whose telnet code (for example `BD`) or name contains one of
  these, without case. The names are the user name, the default name and the name that the device on the input reports,
  from the second connection and the web interface. A source without a name from the receiver, a source that is not on
  the source names page, a source that the receiver does not report as in use, and a tuner source without a tuner are
  not selected either. Without the second connection or the source names page of the web interface, no source is
  selected. No source is selected either while the receiver adds a level above 0 dB to the analog or the digital inputs,
  or when these levels are not known: another source could then be louder at the same volume. Without this option and
  without names in `tools/remote-sim/avoid-sources.txt`, the checks select no source: the `source` and `media_info`
  checks are skipped, `heos` does not ask you to start HEOS Music, `signal` asks you to set the start source back if it
  changed, `http_mode` does not change the source, and the simple command check sends no source or tuner command. If the
  source at the start may not be selected, the checks select no source either, because they select the start source
  again after the change. `signal` and the Bluetooth part of `media_info` then ask you to set it back, and `heos` is
  skipped. A tuner command of the simple command check also needs a tuner source on the receiver, and every tuner source
  to be one that may be selected. The simple command check sends the source and tuner commands after the other commands.
- The names in `tools/remote-sim/avoid-sources.txt` are always added to `--avoid-sources`, separated by commas or new
  lines. Git ignores this file, so it can hold the real names of your sources. Write one short name per entry, for
  example `Oppo`, and no comments: the whole entry must be part of a name or code of the source.
- `--kinds` and `--avoid-sources` can be given more than once; the lists are joined. A missing value, a value without
  names and a value that starts with `-` are refused. Spaces around names are removed.
- Only one receiver address is allowed.
- `receiver_restart` runs only with `--only receiver_restart`. It sends `NETWORK_RESTART` to the receiver and checks
  that the driver connects again (at most 3 minutes) and shows the values of the receiver.
- `ws_idle` runs only with `--only ws_idle`. Another WebSocket connection to the driver sends nothing for 45 s and must
  then still get an answer to a request. The driver closes a connection after 40 s without a message, so only the
  answers to the pings of the driver keep it open.
- `--host-name <name>`: the host name of the receiver for the `address_lookup` check.
- `--no-build` does not build the driver: the tool starts the driver binary as it is. `--driver` does the same.
- `--no-power` skips the power check. The other checks still switch the receiver on if it is off.
- `--no-oracle` does not open the second connection. The values are then not compared with the receiver. Without the
  second connection the volume cannot be read from the receiver, so the checks do not switch the receiver on: a check
  that needs a power on is skipped. The `power`, `volume`, `sound_mode`, `select_sweep`, `extra_entities`,
  `settings_restore`, `simple_commands`, `signal`, `media_info` and `media_keys` checks are also skipped, `heos` does
  not ask you to start HEOS Music, and no source is selected. The `mute` check then uses the volume of the driver.
- `--driver <path>`, `--port <port>` and `--commands <path>` change the driver binary, the port and the command table.
  With `--no-build`, the default driver binary is `target/release/uc-intg-denonmarantz`. The default command table is
  `resources/commands.tsv`. Both are in the repository folder, so the tool works from any folder.
- `RUST_LOG` is added after the default driver log filter. A level in it replaces the default level for the same name,
  for example `denon_marantz=trace,denon_marantz_heos=trace`. The checks read debug lines of the driver, so do not set
  `uc_intg_denonmarantz` or `denon_marantz` below `debug`. Only the `debug_log` check starts the driver without
  `RUST_LOG`, because `RUST_LOG` overrides the debug log option of the setup. It sets `settings.json` back at the end.
  The trace shows the full telnet and HEOS lines only if the driver is built with `cargo build --release --features
  sensitive-logs`. Start the tool with `--no-build` then, else its own build replaces that binary. These lines can hold
  titles and account names: do not share such a log.

The results go to `test-results/remote-sim-results-<time>.md` in the repository folder. To check only the selects and
the switches:

```shell
cargo run --release --manifest-path tools/remote-sim/Cargo.toml -- <receiver address> \
  --only selects,select_sweep,extra_entities --no-power
```

CI checks the format of the tool, runs clippy and runs its unit tests, but not the checks against a receiver. Run the
same checks after you change the tool:

```shell
cargo fmt --all --check --manifest-path tools/remote-sim/Cargo.toml
cargo clippy --locked --all-targets --manifest-path tools/remote-sim/Cargo.toml -- -D warnings
cargo test --locked --manifest-path tools/remote-sim/Cargo.toml
```

## License

[MPL-2.0](LICENSE). Parts of the code come from the
[Home Assistant integration](https://github.com/unfoldedcircle/integration-home-assistant) by Unfolded Circle,
which uses the same license.
