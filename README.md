# Denon and Marantz integration for Remote Two/3 (Rust)

Integration driver for the [Unfolded Circle](https://www.unfoldedcircle.com/) Remote Two and Remote 3.
It controls Denon and Marantz network receivers with the `denon-marantz` library.

The driver is an alternative to the Python integration that is part of the Remote software. It has its own
driver id (`denonavr_rs`), because the Python integration cannot be stopped or uninstalled. The `config.json`,
the entity names and the command names are the same, so receivers that are set up with the Python integration
work without a new setup. The driver adds what it finds on the receiver to `config.json`: the serial number,
the MAC address, the receiver type, the HTTP read method, if the HDMI output can be read over HTTP, the number of quick selects, the values
that the receiver lists for HTTP, and if the receiver has HEOS. The Python integration ignores them.

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
  and HD radio. On receivers with HEOS, network audio shows what HEOS plays, with position and length, and
  play, pause, stop, next and previous go to HEOS. Volume and mute stay on the receiver: HEOS shares one
  volume for all zones.
- **Remote**: the simple commands of the Python integration (about 370), 15 UI pages, button mapping,
  repeat and button sequences. Text of up to 28 characters is sent to the receiver as it is.
- **Selects**: sound mode, input source, reference level, dynamic volume, dimmer, eco mode. With telnet also
  monitor output, speaker preset, picture mode, digital codec and, if the receiver has Dirac, the Dirac filter
  (only the slots that have a filter).
  With telnet, the sound mode select offers the modes that the receiver lists for the current input.
- **More entities** (with telnet, some also with HTTP only, see [Requirements](#requirements)): switches for
  tone control, Dynamic EQ, the three trigger outputs and the Bluetooth transmitter; numbers for bass and
  treble; selects for the Audyssey MultEQ mode, the quick select (smart select on Marantz), the tuner band and
  the Bluetooth output; a sensor for the active Dirac filter. A quick select can change the volume. The quick
  select, its simple commands and its UI page offer only the slots that the receiver has.
- **Sensors**: volume, sound mode, input, mute, dimmer and eco mode. With telnet also sleep timer, audio delay,
  audio and video signal, channels, resolution and bandwidth, video output port, HDR, pixel depth, FRL,
  color space (with BT.2020, for example `4:2:2/BT.2020`), picture mode, tuner frequency, all zone stereo and
  digital codec. Some signal sensors also exist with HTTP only.

With telnet, all entities exist on every receiver, except the Dirac select and sensor, which need a
receiver with Dirac. A value that the receiver does not have, for example the picture mode
of a newer receiver or the Audyssey settings of a receiver without Audyssey, shows `--`. A setting that the
receiver does not accept in its current state, for example the reference level without Dynamic EQ or the tone
control with a Dirac filter, is ignored. The driver answers OK, so the Remote shows no error, and writes a line
to the log that says why.

## Compared to the Python integration

- The receiver is read in a fraction of a second after the Remote wakes up. The values of telnet and the web
  pages follow in the background.
- With telnet, values come from the receiver as events. The signal info is not read on a timer. The setup has
  an option to read it every N seconds for receivers that do not report it.
- Entities keep their state while the Remote sleeps.

## Network and wake-up

- The driver connects to the last address first. If another receiver or none answers there, the driver
  searches the network for its MAC address or serial number and saves the new address.
- After the Remote wakes up, the network of the Remote can be down for a short time. The driver tries again
  with a growing delay for up to 3 seconds. A connect step without an answer in 0.5 seconds is started again
  once.
- A command that comes while the first connect runs waits for it, at most 6 seconds. If the receiver is not
  reachable, a command fails at once.

## Requirements

- A Denon or Marantz network receiver.
- The receiver and the Remote are on the same network, or you enter the address of the receiver in the setup.
- Network control is on in the receiver, also in standby. A fixed IP address (reservation in the router)
  is recommended.
- Some old receivers allow only one telnet connection. If telnet does not work for you, use the connection
  mode _Use HTTP connection_. In this mode the driver reads the receiver every 10 seconds and after each
  command. All receivers have the media player, the remote, 6 selects (sound mode, input source, reference
  level, dynamic volume, dimmer, eco mode) and 7 sensors (volume in dB, absolute volume, sound mode, input
  source, mute, dimmer, eco mode). Receivers that the driver reads with the newer HTTP interface also have
  the MultEQ select, the tone control and Dynamic EQ switches, and the bass and treble numbers. Models from
  2016 and later also have the sensors for the audio signal, sampling rate, HDMI video in and out, and the
  input and output channels. Receivers that report the HDMI output over HTTP also have the monitor output
  select and sensor. If the receiver lists the values it can send over HTTP, the driver offers only those:
  tone control, bass and treble; MultEQ and Dynamic EQ; audio signal and sampling rate. The sound mode select
  offers the sound mode groups, for example `MOVIE`. While the receiver is busy, mostly in the first seconds
  after a connect, a changed dimmer or eco mode can take up to 10 seconds to show.

Changing the connection mode does not remove entities. If you add entities while using telnet and then change to
HTTP only, the entities that need telnet stay in the integration as unavailable. Remove them in the web
configurator of the Remote.

## Install on the Remote

1. Download `uc-intg-denonmarantz-rs-<version>-aarch64.tar.gz` from the
   [releases](https://github.com/henrikwidlund/uc-intg-denonmarantz/releases).
2. Web configurator: Integrations > Add new > Install custom. Choose the file. Remove the receiver from the
   Python integration, so that only one integration is connected to it.
3. To keep the old setup, copy the `config.json` of the Python integration to the configuration folder of
   the driver. Otherwise start the setup. Leave the address empty to search the network. If the setup fails,
   run it again. The receiver must send its description.
   With a copied `config.json`, the driver checks for HEOS at the first connect. If the receiver has HEOS,
   add the media player entity again after that connect to get the position and length.

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
| `UC_SETUP_TIMEOUT`         | `300`             | Setup timeout in seconds. `0` disables it.                      |
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
cargo test --locked
cargo deny check
```

`tools/manual_test.sh` runs the tests that need a real Remote or a person, one step at a time: the setup pages,
the entity lists, the buttons, the sleep of the Remote and a real signal. It saves the results to
`manual-test-results-<time>.md`. Run `tools/remote-sim` (below) for everything else.

`tools/remote-sim` is a program that acts as the Remote and runs most of these tests by itself with a real
receiver. It opens a second connection to the receiver and checks that every select and sensor shows what the
receiver has, after each change. It also sends the simple commands of the remote entity, but not the
triggers and the remote lock. It starts the driver without mDNS, so it needs no root rights. The checks switch
the receiver on and off and change source, sound mode and some settings, and restore them. While the simple
commands are sent, the sound can stop for some seconds, for example when HDMI audio goes to the TV. The
settings that they change are saved in `remote-sim-restore.txt` first. If a run stops before they are set
back, the next run sets them back at its start. While the file is still there, the checks that change these
settings are skipped. A source change stops HEOS and AirPlay, and they do not start
again. The volume never goes above -40 dB. The `heos` check compares
the media player with a second HEOS connection, pauses and plays once. Start HEOS Music on the main zone
before the run, else the check is skipped.

```shell
cargo build --release
cargo run --release --manifest-path tools/remote-sim/Cargo.toml -- <receiver address>
```

Options:

- `--only a,b` and `--skip a,b` select checks by name. `--only` must also name the first checks
  `metadata,setup,entities,states`: the other checks need them.
- `--kinds a,b` limits the select sweep to these selects, for example `dirac_filter`.
- `--avoid-sources a,b`: the checks never select a source whose name contains one of these, without case.
  The simple command check then sends no source commands.
- `--no-power` skips the power check. The other checks still switch the receiver on if it is off.
- `--no-oracle` does not open the second connection. The values are then not compared with the receiver.
- `--driver <path>`, `--port <port>` and `--commands <path>` change the driver binary, the port and the
  command table.
- `RUST_LOG` is passed to the driver, for example `info,denon_marantz=trace,denon_marantz_heos=trace`.
  The trace shows the full telnet and HEOS lines only if the driver is built with
  `cargo build --release --features sensitive-logs`. These lines can hold titles and account names: do not
  share such a log.

The results go to `remote-sim-results-<time>.md`. To check only the selects and the switches:

```shell
cargo run --release --manifest-path tools/remote-sim/Cargo.toml -- <receiver address> \
  --only metadata,setup,entities,states,selects,select_sweep,extra_entities --no-power
```

## License

[MPL-2.0](LICENSE). Parts of the code come from the
[Home Assistant integration](https://github.com/unfoldedcircle/integration-home-assistant) by Unfolded Circle,
which uses the same license.
