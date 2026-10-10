# open-wbg-service

Independent Wooting background service for Linux, with native **Niri, Sway, and
Hyprland** integration. It switches keyboard profiles when the focused application
changes and supplies Linux data for supported Light Indicator effects.

[Wootility](https://wootility.io) remains the profile editor. Configure profiles and
app links there; this service reads them from the keyboard and runs without
Wootility staying open. It replaces the proprietary background service, not
Wootility itself.

- Native compositor IPC, including identities for XWayland and Steam/Proton games.
- Keyboard-resident app links, with optional TOML rules that take precedence.
- System, audio, and battery data providers; optional authenticated Discord RPC.
- A loopback Wootility compatibility API and a systemd user service.

Profile changes are volatile. The service does not flash profiles or firmware.
Do not run it alongside the proprietary background service or another
profile-switching daemon.

## Requirements

- Linux with Niri, Sway, or Hyprland. There is no standalone X11 backend or
  GNOME/KDE integration; XWayland windows are identified through the supported
  compositor.
- A modern Wooting keyboard exposing the `FF55` control interface, with
  App Linking-capable firmware **2.14 or newer**.
- A current stable Rust toolchain, a C compiler, `pkg-config`, and libudev headers
  to build from source.
- systemd user services for the optional autostart installer. Foreground use does
  not need a systemd unit, but must run in your desktop user session with
  `XDG_RUNTIME_DIR` set.

Optional: `pactl` for speaker volume/mute data, using PulseAudio or PipeWire's
PulseAudio server; `notify-send` and a notification daemon for profile-change
notifications (`notify = true`). Light Indicator effects require compatible
hardware; the 60HE v2 has no lightbar.

## Install

Install the native build dependencies for your distribution:

```sh
# Fedora
sudo dnf install gcc pkgconf-pkg-config systemd-devel

# Debian / Ubuntu
sudo apt install build-essential pkg-config libudev-dev
```

With Rust installed, run from this repository's root:

```sh
cargo install --path . --locked
open-wbg-service --help
```

Cargo normally installs the executable in `~/.cargo/bin`; ensure that directory is
on your `PATH`. Do not run Cargo or the service as root.

### Keyboard permissions

If your user cannot access the keyboard's hidraw interface, install the supplied
udev rule from the repository root:

```sh
sudo install -m 644 packaging/70-open-wbg-service.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
```

Disconnect and reconnect the keyboard. The rule grants the active local seat user
access; it does not make devices world-writable. See the
[udev rule](packaging/70-open-wbg-service.rules).

## Quick start

Stop any competing background service first. In Wootility's **My Profiles** tab,
create or select linked profiles and associate them with Linux applications.
Windows-only executable paths need Linux associations.

Run these commands from a terminal inside your compositor session:

```sh
open-wbg-service devices
open-wbg-service profiles
open-wbg-service run --dry-run --duration 10
```

The dry run reads keyboard state and logs profile decisions without activating
profiles or sending lightbar updates. Focus an associated application during the
run to inspect its selection.

Then start normal operation:

```sh
open-wbg-service run
```

By default, the service restores the previous onboard profile on graceful
SIGINT/SIGTERM shutdown, including Ctrl+C. Configuration is optional: existing
keyboard app links work with the defaults.

### Start automatically

Stop the foreground instance before installing autostart. Run the installer from
your compositor's terminal, **not with sudo**:

```sh
open-wbg-service install-user --start
systemctl --user status open-wbg-service.service
journalctl --user -u open-wbg-service.service -f
```

The installer records the current executable and configuration paths, creates a
default config if one is missing, imports the current compositor environment, and
enables a user unit tied to `graphical-session.target`. Your desktop session must
activate that target and expose its compositor environment to the user manager on
future logins. Keep the installed executable at the recorded path.

To stop the service, or remove its generated unit:

```sh
systemctl --user stop open-wbg-service.service
open-wbg-service uninstall-user
```

`uninstall-user` disables autostart and removes the unit; it keeps the executable
and settings.

## Configuration

The default path is `$XDG_CONFIG_HOME/open-wbg-service/config.toml`, falling back
to `~/.config/open-wbg-service/config.toml`. A missing default config uses built-in
defaults. An explicit `--config PATH` must point to an existing file.

Use the [annotated example](examples/config.toml), or print it locally:

```sh
open-wbg-service example-config
open-wbg-service --config examples/config.toml check
```

Copy the example to your config path only if you need overrides; do not overwrite
existing settings. This is a minimal custom rule for a profile already stored on
the keyboard:

```toml
backend = "auto"

[[rules]]
profile = "linked:0"
app_id = "^steam_app_730$"
```

Inspect actual window identities before writing rules:

```sh
open-wbg-service watch --duration 10
```

- Profile indices are zero-based: `onboard:0` is P1; `linked:0` is the first linked
  profile. Rules never create or flash profiles.
- The first matching rule wins, before keyboard-resident app links. All selectors
  within a rule must match.
- `app_id`, `title`, and `executable` are case-sensitive regular expressions; use
  `(?i)` for case-insensitive matching. `steam_id` is numeric; `serial` is an exact
  keyboard serial filter. A rule needs at least one window selector, not just
  `serial`.
- An empty top-level `serials` list selects all supported connected keyboards.
- `backend = "auto"` checks `NIRI_SOCKET`, then `SWAYSOCK`, then
  `HYPRLAND_INSTANCE_SIGNATURE`. Use `--backend niri`, `sway`, or `hyprland` with
  `run` or `watch` to select one explicitly; its session environment is still
  required.
- Focus events trigger profile decisions immediately. By default, metrics and
  current-profile checks run every 1,000 ms, and keyboard profile/app-link catalogs
  refresh every 10 seconds.
- `fallback_profile = 0` is the onboard index used when starting in a linked
  profile without a known previous onboard profile. `restore_on_exit = true`
  enables restoration on graceful shutdown.

Validate changes and reload rules/providers in a running user service:

```sh
open-wbg-service check
systemctl --user kill --kill-whom=main --signal=HUP open-wbg-service.service
```

Use the same `--config PATH` for `check` if the service uses a custom file. Invalid
reloads retain the previous settings. Restart after changing the backend or API
settings:

```sh
systemctl --user restart open-wbg-service.service
```

## Wootility and data providers

By default, the compatibility API listens on `127.0.0.1:50052`. Allow Wootility's
local/loopback network permission in your browser. Browser requests are accepted
from `https://wootility.io`, `https://beta.wootility.io`, and
`https://v5.wootility.io`; do not expose the API on a public interface.

```sh
open-wbg-service status
curl http://127.0.0.1:50052/status
open-wbg-service metrics
```

`status` requires a running daemon with its API enabled. `metrics` samples the
configured Linux providers directly. Use `run --no-api` or `api = false` to disable
the API; profile switching continues, but Wootility connectivity and `status` are
unavailable.

The default `enabled_sources` are `system_info_source`, `system_volume`, and
`system_battery_source`. They provide CPU/RAM/swap/disk/network/temperature,
speaker level/mute, and battery level/charging data where available. Configure
Light Indicator effects in Wootility; the service uses firmware-provided
subscription slots rather than assuming fixed mappings. Migrated temperature or
battery effects may need their sources reselected because Linux sensor keys differ
from proprietary platform keys.

### Optional Discord integration

Discord mute/notification data requires **your own approved or tester-enabled
Discord RPC application** and a registered redirect URI. It cannot bypass Discord's
approval requirements and does not use Wooting credentials or Discord user/session
tokens.

Set `OPEN_WBG_DISCORD_CLIENT_SECRET` in the environment, never as a command-line
argument or in TOML. Then use `open-wbg-service discord-login --help` for the
`--client-id` and `--redirect-uri` arguments. Successful authorization stores the
OAuth token in a private `0600` file and enables `discord_source` in the config.
Reload or restart the daemon afterward; rerun authorization when it expires.

### Compatibility limits

Native Niri profile switching has been verified on a 60HE v2. Wootility 5.4.2
connectivity and data-source controls have also been verified. Real lightbar output,
Discord OAuth, and detailed Wootility effect/app pickers still need end-to-end
verification. Sway and Hyprland backends are implemented, but that Niri hardware
verification does not establish equivalent coverage for them.

The undocumented `os/kind` numeric value is intentionally omitted until its Linux
wire value is verified. Wootility's proprietary self-updater is not supported;
update this service through your package manager or a new source build instead.

## Commands and troubleshooting

| Command | Purpose |
| --- | --- |
| `devices` | List supported HID control interfaces without changing keyboard state. |
| `profiles [--serial SERIAL]` | Read stored profiles and app links; stop the daemon first. |
| `switch onboard:0 [--serial SERIAL]` | Manually activate an existing profile; stop the daemon first. Also accepts `linked:INDEX`. |
| `watch [--backend BACKEND]` | Print focus events and resolved application identities as JSON. |
| `apps` | List installed XDG desktop and native/Flatpak Steam applications. |
| `metrics` | Sample enabled Linux providers as JSON. |
| `status` | Read the running daemon's loopback status endpoint. |
| `check` | Validate configuration without starting workers or changing the keyboard. |

Run `open-wbg-service COMMAND --help` for command-specific options.

- **No keyboard / permission denied:** check the USB connection, serial filters,
  firmware compatibility, and udev permissions; reconnect after installing rules.
- **No supported compositor socket:** run inside Niri/Sway/Hyprland. For autostart,
  import the live session environment into `systemd --user`; `install-user` does
  this for the session in which it runs.
- **Device lock held:** stop the running service before using `profiles`, `switch`,
  or another `run` instance. Do not remove an active process's lock file.
- **Port 50052 already in use:** stop the competing background service. A custom
  `api_port` changes the listener, but Wootility expects the default port.
- **Wootility cannot connect:** check `status`, ensure the API is enabled, and allow
  the browser's local-network permission.
- **Profiles do not switch:** inspect `watch`, check Linux app associations and
  rule targets, then try a dry run with the normal daemon stopped.
- **Missing audio data:** check that `pactl` can access your PulseAudio-compatible
  server. A keyboard without external lightbar telemetry can still use app linking.

Logs go to stderr and `$XDG_STATE_HOME/open-wbg-service/logs/`, falling back to
`~/.local/state/open-wbg-service/logs/`. Seven daily log files are retained.
`RUST_LOG=debug open-wbg-service run` enables more detail for a foreground instance.

## Build and update

To build without installing:

```sh
cargo build --release --locked
./target/release/open-wbg-service --help
```

To reinstall from an updated source checkout:

```sh
cargo install --path . --locked --force
systemctl --user restart open-wbg-service.service
```

Restart only if you use the user service. Do not use Wootility's proprietary
updater to update this executable.

## License

[MIT](LICENSE).
