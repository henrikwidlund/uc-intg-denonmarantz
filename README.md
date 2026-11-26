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

- **Media player**: power, volume and mute, input source, sound mode, navigation, menu, options, info,
  channel up and down, play and pause, next and previous. Title, artist, album and cover for network audio
  and HD radio. Receivers with `CommApiVers` 0301 or higher have HEOS. On receivers with HEOS, network audio
  shows what HEOS plays, with position, length, repeat and shuffle, and play, pause, stop, next, previous,
  repeat and shuffle go to HEOS. Volume and mute stay on the receiver: HEOS shares one volume for all zones.
  The Remote must be able to reach the receiver on TCP port 1255 (HEOS) and 8015 (covers from AirPlay and
  USB, seen on an AVC-A10H). Volume up and down use the step of the receiver (0.5 dB). The volume step of a
  Python integration configuration is not used. A volume above the top of the scale that the receiver lists
  is set to the top.
- **Remote**: the simple commands of the Python integration (about 370), `BT_LEVEL_UP`/`DOWN`,
  `TACTILE_CHANNEL_*` (the channel of the tactile transducer, `CVTTR`), commands of older receivers (see
  [Older receivers](#older-receivers)), up to 15 UI pages, button mapping, repeat and button sequences. Text
  of up to 28 characters is sent to the receiver as it is.
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
  time the receiver is on and the tuner or network audio plays, and keeps them in `config.json`. Until then the selects show
  `Preset n` and `Favorite n`. On older receivers a tuner preset is selected only after the receiver has
  shown if its presets are `01` to `56` or `A1` to `G8`; the driver saves this. Receivers with 0301 and
  higher use `01` to `56`. On some older US models the receiver does not report the tuner frequency and
  preset.

Commands with a number are sent as raw text with the remote entity:

| Text | What it does |
|---|---|
| `NSB00` to `NSB55` | Play network favorite 1 to 56, 1 to 36 on Denon 0250 and 0300 (receivers without HEOS) |
| `NSC00` to `NSC55` | Save what plays as network favorite 1 to 56, 1 to 36 on Denon 0250 and 0300. This replaces the favorite. The driver then reads the names again. |
| `TFAN` + 6 digits | Tune the tuner, in hundredths: of MHz for FM (`TFAN010110` = 101.10 MHz), of kHz for AM (`TFAN105000` = 1050 kHz) |
| `TPANMEM01` to `TPANMEM56` | Save the tuned station as tuner preset 1 to 56 (`TPANMEMA1` to `TPANMEMG8` on receivers with lettered presets). The driver then reads the names again. |
| `TFHD` + 6 digits | Tune the HD Radio tuner, in the same way (North America models) |

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
  for the name. When you add a receiver and the setup cannot look up the name, it connects with the
  name; if that fails, nothing is saved. When you change the address of a configured receiver, the setup saves
  the first IP address if the receiver answers at none, and the name as the address if it cannot look up the
  name. If `address` in `config.json` is a host name and `host` is not set, the driver moves the name to `host`
  when it saves a new address.
- After the Remote wakes up, the network of the Remote can be down for a short time. The driver tries again
  for up to 3 seconds. A connection to the receiver that does not open in 0.25 seconds is tried again. A start or
  lookup of the receiver that takes more than 3 times as long as the fastest of the last 5 starts (at least 0.2
  seconds) is started again, at most 4 times. There is no such limit for the first connect, or if that start took
  more than about 0.7 seconds. If telnet or HEOS cannot connect after wake-up, they are tried again every 0.05
  seconds for 3 seconds, then less often (HEOS only if the receiver has it).
- A command that comes while the first connect runs waits until it ends, within the 9 seconds below. It is not
  sent if the Remote goes to standby or the setup connects again during the wait. Commands that do not need the
  state of the receiver (power, volume, mute, cursor keys, back, info, channel up and down, raw text, and next,
  previous and stop on receivers without HEOS) are sent when the receiver is known, before the start ends.
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
| `UC_API_MSG_TRACING`       | off               | Log WebSocket messages: `all`, `in` or `out`.                   |
| `RUST_LOG`                 | `info`            | Log level, for example `info,denon_marantz=debug`.              |

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
See `.github/workflows/build.yml`. A `v*` tag creates a GitHub release with the archive.

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

`tools/manual_test.sh` runs the tests that need a real Remote or a person, one step at a time: the setup pages,
the entity lists, the buttons, the sleep of the Remote and a real signal. It saves the results to
`test-results/manual-test-results-<time>.md`. Run `tools/remote-sim` (below) for everything else.

`tools/remote-sim` is a program that acts as the Remote and runs most of these tests by itself with a real
receiver. It opens a second connection to the receiver and checks that every select and sensor shows what the
receiver has, after each change. It also sends the simple commands of the remote entity, but not the triggers,
the remote lock and the LFE level of the 7.1 input. It starts the driver without mDNS, so it needs no root
rights. The checks switch the receiver on and off and change source, sound mode and some settings, and restore
them, also when a check fails. The `mute` check runs only if the mute state is known, the receiver is not
muted, its volume is at or below -40 dB and the driver shows the same volume as the receiver. An unmute comes
only after the volume is at or below the volume before the change and at or below -40 dB (the volume of the
receiver, or of the driver without the second connection). At the end, `restore_power` sets the power and the
mute state back to the start and, with the second connection, lowers the volume to the start volume (not above
-40 dB) if it is higher. While the simple commands are sent, the sound can stop for some seconds, for example
when HDMI audio goes to the TV. The settings that they change and the source are saved in
`remote-sim-restore.txt` in the repository folder first. A source that may not be selected is saved only as a
line that is never sent. If the source is not read, the check is skipped. If a run stops before they are set
back, the next run sets them back at its start. It sets the source back only if the main zone is on, exactly one
source was saved, and both the saved source and the current source may be selected (see `--avoid-sources`). Some
settings are kept for each source, so if the source cannot be set back and is not the saved source already,
nothing is set back and the file stays. This is also true at the end of a check, and when the source must be set
back while the main zone is off. While the file is still there, the checks that change these settings are
skipped. A source change stops HEOS and AirPlay, and they do not start again. The checks never set the volume
above -40 dB. A power on can start with a higher volume of the receiver: if it is above -40 dB, it is lowered to
-40 dB at once. The `heos` check compares the media player with a second HEOS connection, pauses and plays once.
Start HEOS Music on the main zone before the run, else the check is skipped.

```shell
cargo build --release
cargo run --release --manifest-path tools/remote-sim/Cargo.toml -- <receiver address>
```

Options:

- `--only a,b` and `--skip a,b` select checks by name. Both can be given more than once. Unknown step names
  are refused. `--only` also runs `setup`, `entities` and `states`, because the other checks need them, and
  `--skip` refuses them. `restore_power` and `log` always run at the end, also with `--only` and `--skip`.
  If `metadata`, `setup`, `entities` or `states` fails, only `restore_power` and `log` run after it.
- `--kinds a,b` limits the select sweep to these selects, for example `dirac_filter`.
- `--avoid-sources a,b`: the checks never select a source whose telnet code (for example `BD`) or name contains
  one of these, without case. The names are the user name, the default name and the name that the device on the
  input reports, from the second connection and the web interface. A source without a name from the receiver, a
  source that is not on the source names page, a source that the receiver does not report as in use, and a tuner
  source without a tuner are not selected either. Without the second connection or the source names page of the
  web interface, no source is selected. No source is selected either while the receiver adds a level above 0 dB
  to the analog or the digital inputs, or when these levels are not known: another source could then be louder
  at the same volume. Without this option and without names in `tools/remote-sim/avoid-sources.txt`, the checks
  select no source: the `source` check is skipped, `http_mode` does not change the source, and the simple
  command check sends no source or tuner command. If the source at the start may not be selected, no source is
  changed either, because the checks select the start source again after the change. A tuner command of the
  simple command check also needs a tuner source on the receiver, and every tuner source to be one that may be
  selected. The simple command check sends the source and tuner commands after the other commands.
- The names in `tools/remote-sim/avoid-sources.txt` are always added to `--avoid-sources`, separated by commas
  or new lines. Git ignores this file, so it can hold the real names of your sources. Write one short name per
  entry, for example `Oppo`, and no comments: the whole entry must be part of a name or code of the source.
- `--kinds` and `--avoid-sources` can be given more than once; the lists are joined. A missing value, a value
  without names and a value that starts with `-` are refused. Spaces around names are removed.
- Only one receiver address is allowed.
- `receiver_restart` runs only with `--only receiver_restart`. It sends `NETWORK_RESTART` to the receiver and
  checks that the driver connects again (at most 3 minutes) and shows the values of the receiver.
- `--no-power` skips the power check. The other checks still switch the receiver on if it is off.
- `--no-oracle` does not open the second connection. The values are then not compared with the receiver.
  Without the second connection the volume cannot be read from the receiver, so the checks do not switch the
  receiver on: a check that needs a power on is skipped. The `power`, `volume`, `select_sweep`,
  `settings_restore` and `simple_commands` checks are also skipped, and no source is selected. The `mute`
  check then uses the volume of the driver.
- `--driver <path>`, `--port <port>` and `--commands <path>` change the driver binary, the port and the
  command table. The default driver binary is `target/release/uc-intg-denonmarantz` and the default command
  table is `resources/commands.tsv`, both in the repository folder, so the tool works from any folder.
- `RUST_LOG` is added after the default driver log filter. A level in it replaces the default level for the
  same name, for example
  `denon_marantz=trace,denon_marantz_heos=trace`. The checks read debug lines of the driver, so do not set
  `uc_intg_denonmarantz` or `denon_marantz` below `debug`.
  The trace shows the full telnet and HEOS lines only if the driver is built with
  `cargo build --release --features sensitive-logs`. These lines can hold titles and account names: do not
  share such a log.

The results go to `test-results/remote-sim-results-<time>.md` in the repository folder. To check only the
selects and the switches:

```shell
cargo run --release --manifest-path tools/remote-sim/Cargo.toml -- <receiver address> \
  --only selects,select_sweep,extra_entities --no-power
```

CI checks the format of the tool, runs clippy and runs its unit tests, but not the checks against a receiver.
Run the same checks after you change the tool:

```shell
cargo fmt --all --check --manifest-path tools/remote-sim/Cargo.toml
cargo clippy --locked --all-targets --manifest-path tools/remote-sim/Cargo.toml -- -D warnings
cargo test --locked --manifest-path tools/remote-sim/Cargo.toml
```

## License

[MPL-2.0](LICENSE). Parts of the code come from the
[Home Assistant integration](https://github.com/unfoldedcircle/integration-home-assistant) by Unfolded Circle,
which uses the same license.
