# Testing Den from an agent

What an agent (or a script) can test today without anyone's help, and
what got in the way during the Windows bug hunt of 2026-11-16. The aim
is for every feature to be testable in the background, on a machine
someone is using, through the CLI and the control API alone.

## What works well

* **Isolated instances**: `DEN_HOME=<dir>` gives each instance its own
  profile, key and API. Several apps on one machine join each other's rooms
  over loopback/LAN, which stands in for "a second machine".
* **Control API** (`den api …`): host, join, invite, voice join/mute/deafen,
  `voice.stats` (devices, codec, packet counts, levels), `state.get`
  (now including `via`: direct or relay), messages, theme/layout switching.
* **Screenshots**: `den api ui.screenshot path=…` renders the real window
  even when it's behind other windows. Good for themes, fonts and layout.
* **Display scaling**: `[ui] scale = 1.5` in `config.toml` stands in for
  Windows DPI scaling (egui zoom works the same way as a higher DPI).
* **Shutdown**: `taskkill /PID <pid>` (no `/F`) sends the window a close
  request, the same path as the close button.
* **Integration tests**: `crates/denchat-core/tests/room.rs` runs real
  servers on loopback with no relays.

## What blocked testing, and what would fix it

| Blocker | What happened | Proposed fix |
| --- | --- | --- |
| **No way to click the GUI in the background** | Real mouse input (`SendInput`/`mouse_event`) goes to whatever window is in front: a test click landed in the user's game. Posting `WM_LBUTTONDOWN` to Den's window is ignored by winit. So menu items such as "Copy invite link" couldn't be clicked end to end. | Add `ui.input {events: [...]}` to the control API, which queues egui pointer/key/text events into the next frame (Den runs them like real input, with no focus or cursor changes). Or turn on eframe `accesskit` and drive it through UI Automation. Also, every panel action should have an API method (see AGENTS.md). |
| **The clipboard is the user's** | Checking a copy meant writing to and reading the system clipboard, which overwrote whatever the user had copied. | `ui.clipboard.get` (what the app last copied) and/or `DEN_CLIPBOARD=<file>` to send copies to a file during tests. |
| **Voice needs real devices** | Voice tests had to use real devices. Silent virtual ones (Oculus, Steam Streaming) proved packets flow, but levels, VAD and noise couldn't be measured. Playing a tone into VB-Audio Cable was too risky (it may feed the user's Discord). | Built-in test devices: `input_device = "test:sine:440"` / `"test:file:<wav>"` and `output_device = "test:null"` / `"test:capture:<wav>"`, then compare levels/SNR in `voice.stats` or the captured WAV. |
| **Can't force a relay path** | `via` only ever showed `direct` on one machine (iroh punches through at once), so the relay case and the firewall-blocked case went untested. | A test switch to turn off direct paths (`[net] direct = false` / `DEN_NO_DIRECT=1`), so a relay-only link can be tested on demand. |
| **Firewall prompt can't be observed** | Windows Defender showed no prompt and made no rule for `den.exe` (public profile, user notifications on); the user was in a fullscreen game. No way to check this headlessly. | Document the expected rule; optionally add `den doctor`, which reports firewall rules for the exe, the network profile, port mapping, reach and relay latency as JSON. |
| **GUI windows appear on the user's desktop** | Each test instance opens a real 1100×720 window; that can steal focus or get in the way. | `DEN_WINDOW=hidden` (or offscreen), keeping screenshots working, for background test runs. |
| **Console events need hand-made helpers** | Ctrl+C and console-close to a process in its own console need `AttachConsole` + `GenerateConsoleCtrlEvent` / `WM_CLOSE` to the console window (PowerShell + C# here). | Ship a small `den-test` helper (or `scripts/windows/*.ps1`) for: send Ctrl+C, close the console, check whether a process has a console. |
| **System DPI can't be changed** | Changing display scaling would change the user's monitor. | `[ui] scale` already works; also add a `--scale` flag / `DEN_SCALE` so tests don't have to edit config. |
| **Real second machine** | Only same-machine instances were possible. | A remote runner (another host with Den and an SSH/agent hook) for real NAT/firewall/LAN tests. |
| **Stale docs** | AGENTS.md said `den host --bind localhost`, which no longer exists. | Fixed. |

## Recipes used

```sh
# Two apps on one machine, both in a room
DEN_HOME=/tmp/a den --nick alice host &
DEN_HOME=/tmp/a den api session.invite            # -> den://…
DEN_HOME=/tmp/b den --nick bob join 'den://…' &
DEN_HOME=/tmp/b den api state.get                 # conn, via, latency_ms

# Voice on silent virtual devices (config.toml in each home)
# [voice]
# input_device = "<a silent input from `den devices`>"
# output_device = "<an output nobody listens to>"
# mode = "open"
DEN_HOME=/tmp/a den api voice.join channel=lounge
DEN_HOME=/tmp/a den api voice.stats              # tx/rx packets, codec

# Fonts / DPI
DEN_HOME=/tmp/a den api message.send text='😀 你好 مرحبا'
DEN_HOME=/tmp/a den api ui.screenshot path=/tmp/shot.png
```
