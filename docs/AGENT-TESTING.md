# Driving and testing Den from an agent

Anything a person can do in the app, an agent can do in the background
from a shell. It can read what's on screen, click, type, press keys,
scroll, hover, take screenshots, and resize or minimise the window. This
works on the user's running app (it never moves their mouse or takes
focus) and on throwaway **sandboxes** that have no window at all. Output is
short plain text, so an agent spends few tokens reading it.

```sh
den sandbox start a                    # a headless Den with its own home, hosting a room
den -S a ui tree                       # what's on screen, one widget per line
den -S a ui click "Room menu"
den -S a ui click "Copy invite link"
den -S a ui clip                       # what it copied (never the user's clipboard)
den -S a ui type "hello" --enter       # into the composer
den -S a ui shot /tmp/a.png            # then look at the PNG
den sandbox stop a --rm
```

Drop `-S a` to drive the user's own running app instead. Every `den ui`
command is a thin wrapper over the control API (`ui.*` in
[API.md](API.md#driving-the-interface)), so scripts and mods can do the
same over the socket.

**Turning it off.** `[modules] agent_control = false` in `config.toml`
makes the app refuse every call that reads or drives its interface:
`den ui …`, `ui.*` and `app.quit`. Mods and the rest of the API, including
`den status` and `den wait`, keep working, and `den status` shows `agent
control off`. Sandboxes always have it on, even with `--copy-config`. It's
a preference, not a security boundary: anything that can edit `config.toml`
as the user can turn it back on.

## Reading the screen

```text
$ den -S a ui tree
window 1100x720 "a's room — Den" (headless)
core.header
  @37 button "Room menu: a's room · 2 online · you host"
core.voice
  @38 button "Voice settings"
  @39 button "Mute"
core.channels
  @43 button "#general" on
  @45 button "Join lounge (b)"
core.composer
  @51 textbox "Message #general" value="" focused
  @52 button "Send" disabled
example.dice/history
  @53 button "d20"
```

* Widgets are grouped by the panel that drew them (layout ids such as
  `core.members` or `my.mod/panel`). Popups and menus appear under the panel
  that opened them.
* `value=` is a text box's contents, or a slider's or meter's number
  (`progress "Mic meter" value="0.42"`).
* **Targets.** Any command that takes a target accepts:
  * `@37`: a ref. A ref stays valid while that widget keeps the same text.
    Once the text changes or the widget goes away, the ref stops matching
    and the command fails, so it never quietly lands on another widget.
  * `Join lounge`: text. Den tries an exact match, then a case-insensitive
    one, then a substring.
  * `button:Join`: a role and text.

  If several widgets match, the command fails and lists them. Narrow it
  with `--panel`, `--index N` or a ref.
* `--panel <id>` limits the output to one panel. `--all` includes widgets
  scrolled out of view. `--rects` adds `[x,y wxh]` in points. `--json` gives
  everything as data.
* `den status` summarises the app's state in a few lines: connection, room,
  channels, members, voice and mods. Use it instead of `den api state.get`.
* Screenshots: `den ui shot [path] [--panel id | --target T] [--scale 0.5]`.
  It prints `path WxH renderer`. A visible window is captured from the GPU.
  A minimised, hidden or headless one is drawn on the CPU, so the result is
  the same either way. A cropped or scaled-down shot costs far fewer tokens
  to look at.

## Acting

| Command | |
| --- | --- |
| `den ui click <target \| X,Y> [--right] [--double] [--pointer]` | Click through the widget's accessibility action, or with a synthetic pointer at its centre (a widget scrolled out of view is scrolled to first). |
| `den ui type <text> [--into T] [--enter]` | Type into the focused text box (or focus `T` first). `--enter` sends it. |
| `den ui key <keys…>` | Press keys and chords in order: `Escape`, `Tab`, `ctrl+a Backspace`, `cmd+k`. |
| `den ui scroll <dy> [--dx N] [--over T]` | Scroll by points over a widget. A positive `dy` scrolls down. |
| `den ui hover <target>` | Move the pointer over it. Returns after its tooltip has appeared. |
| `den ui window [--size WxH] [--pos X,Y] [--minimize\|--restore\|--maximize] [--fullscreen] [--show\|--hide] [--focus]` | Print the window's state. The flags change it first. |
| `den ui scale 1.5` | UI zoom, which stands in for display scaling. |
| `den ui clip` | Print the text the app last copied. |
| `den ui quit` | Close the app the way its close button does. |

Every action returns once the frame it caused has been drawn. Run
`den ui tree` straight after a click and the menu it opened is already
listed.

### Waiting

* `den ui wait <target> [--gone] [--timeout 10]` waits until a widget
  appears (or, with `--gone`, disappears). It exits 1 on timeout.
* Every action takes `--wait <target>`: `den ui click Mute --wait Unmute`.
* `den wait <event…> [--where path=value] [--count N] [-- <command>]`
  waits for events from the app and prints them. It only sees events from
  after it starts listening, so put the action that causes the event after
  `--`. Den runs it once it's listening:
  `den wait message.created --where message.text=rolled -- den ui type /roll --enter`.
  Event names are listed in [API.md](API.md#events).

## Sandboxes

`den sandbox start [name]` starts a Den in the background, with its own home
folder (under `$DEN_SANDBOXES`, by default `<tmp>/den-sandboxes`). It waits
until the room is up and prints the invite link. A sandbox:

* runs **headless**, with no window, no GPU and no focus. Add `--window` to
  see a real one.
* uses the **test audio devices** `test:silence` → `test:null` (or
  `--input test:sine:440`, `--output test:capture:<wav>`). It never opens
  the user's microphone or speakers: a real device picked in its Voice
  settings gets a test device instead (`DEN_TEST_AUDIO=1`).
* sends anything it **copies** to `<home>/clipboard.txt`, never to the
  user's clipboard.
* hosts a room, joins one (`--join b`, or `--join '<invite>'`), or stays in
  the lobby (`--lobby`).
* loads mods with `--mod <dir>` (repeatable; they are linked, not copied).
* starts from the user's own setup with `--copy-config` (theme, layout, and
  their mods); `--theme` and `--layout` override those.
* logs to `<home>/den.log`.

You can also:

* drive a sandbox with `den -S <name> …` (or `export DEN_SANDBOX=<name>`).
  `den -S a api …`, `den -S a mods …` and `den -S a config path` all target
  it.
* manage them with `den sandbox list`, `den sandbox stop <names…> | --all
  [--rm]` and `den sandbox path <name>`.

Several sandboxes on one machine can join each other's rooms, which stands
in for "a second machine":

```sh
den sandbox start a
den sandbox start b --join a
den -S a wait message.created --where message.text=hi -- den -S b ui type "hi from b" --enter
den -S b ui click "Join lounge" --wait "Leave voice"
den -S a status                         # members a (you), b [voice:lounge]
```

Without sandboxes, the same switches work on any instance:
`DEN_HOME=/tmp/x den --window headless --size 900x600 host`.

* `DEN_WINDOW=headless` runs with no window.
* `DEN_SIZE=WxH` sets the window size (and doesn't save it).
* `DEN_SCALE=1.5` sets the UI zoom.
* `DEN_CLIPBOARD=<file>` sends copies to a file.
* `DEN_TEST_AUDIO=1` opens only `test:` audio devices.

## Testing a mod end to end

```sh
den mods new ./my-mod --id my.mod            # a starter: /hello and a status panel
den sandbox start t --mod ./my-mod --fresh
den -S t status | tail -1                    # mods my.mod · panels my.mod/status
den -S t ui tree --panel my.mod              # does the panel show what it should?
den -S t ui click "Say hello" --panel my.mod
den -S t wait message.created --where message.text=agent -- den -S t ui type "/hello agent" --enter
den -S t ui shot /tmp/panel.png --panel my.mod
# edit den-mod.toml / mod.py, then:
den -S t api mods.reload
den sandbox stop t --rm
```

* Test it against the user's look with `--copy-config`. For a multiplayer
  mod, add a second sandbox with `--join t` and check that the other side
  sees what it should.
* Problems loading the mod show in `den -S t api mods.list` and in
  `<home>/den.log`.
* Mod panels are made from declarative widgets, so they show up in
  `ui tree` with no extra work.

## Built-in panels: keep them drivable

Stock egui widgets (buttons, labels, text boxes, checkboxes) name
themselves. A widget drawn by hand with `allocate_exact_size` or `interact`
does not, so call `kit::name(&resp, Role::Button, "Join lounge")`
(`kit::name_selected` for a current or selected item). Use the words a
person sees, plus any state that only shows as colour or an icon
(`"b · Muted · lounge"`). This is what the accessibility tree and screen
readers get, too.

## Voice without real devices

Set these in a home's `config.toml` (`[voice] input_device` /
`output_device`), or start a sandbox with `--input` / `--output`.
Sandboxes use `test:silence` and `test:null` by default.

| Device | |
| --- | --- |
| `test:silence` | Input: digital silence. |
| `test:sine:<hz>` | Input: a steady tone, e.g. `test:sine:440`. |
| `test:file:<wav>` | Input: a WAV file, looped. |
| `test:null` | Output: discards everything. |
| `test:capture:<wav>` | Output: writes what it hears to a WAV file. It starts over each time voice starts. |

```sh
den sandbox start a --input test:sine:440
den sandbox start b --join a --output test:capture:/tmp/b.wav
den -S a api voice.join channel=lounge; den -S b api voice.join channel=lounge
den -S b api voice.stats                     # rx_packets climbing, level > 0
```

## Network switches

* `[net] direct = false` (or `DEN_NO_DIRECT=1`) makes an endpoint
  relay-only, with no UDP sockets and no addresses. Setting it on one side
  is enough. `den status` then shows `via relay`.
* `den doctor [--json]` reports:
  * reach and addresses;
  * port mapping (offered / granted);
  * relay latency and health;
  * paths;
  * on Windows, the firewall rules for the exe and the active network
    profile.

## Automated tests

* `crates/denchat-core/tests/room.rs` runs real servers on loopback with no
  relays: rooms, sync, takeover, voice on test devices, relay-only links.
* `crates/denchat-gui/tests/headless.rs` runs the real app headless and
  drives it through the same calls `den ui` uses: find, click, type, menu,
  clipboard, toggle a panel, screenshot, resize, quit.

## What's still hard

| Gap | Status |
| --- | --- |
| Clicking the GUI in the background | **Fixed**: `den ui click/type/key/scroll/hover`, which inject egui input or accesskit actions. No focus, no real mouse. |
| Acting, then waiting for the event it causes | **Fixed**: `den wait <event> -- <command>` listens first, then runs the command. |
| The clipboard is the user's | **Fixed**: `den ui clip`, and `DEN_CLIPBOARD=<file>` (always on in sandboxes and headless). Clicking "Copy" in the user's own app still copies to their clipboard, as it should. |
| Voice needs real devices | **Fixed**: `test:` devices (above). |
| Can't force a relay path | **Fixed**: `[net] direct = false` / `DEN_NO_DIRECT=1`. |
| Test windows on the user's desktop | **Fixed**: `DEN_WINDOW=headless`; sandboxes are headless by default. |
| Screenshots of hidden or minimised windows | **Fixed**: CPU renderer fallback. |
| System DPI can't be changed | **Fixed**: `DEN_SCALE`, `--scale`, `den ui scale`. |
| Firewall prompt can't be observed | `den doctor` reports the rules and the network profile; the prompt itself still can't be seen. The Windows check is untested. |
| Console Ctrl+C / close on Windows | Still needs hand-made helpers (`AttachConsole` + `GenerateConsoleCtrlEvent`). `den ui quit` and `den sandbox stop` cover the normal close path. |
| A real second machine | Still manual. Sandboxes cover same-machine rooms (direct and relay-only); real NAT and firewall need another host. |
| `den serve` with direct off | Needs `--relay-url` (or `[relay] url`, or `[net] relays`); it now refuses to start without one. A relay-only host no longer dials members to check their reach. |
| Headless and windowed driving on Windows/Linux | Built and tested on macOS. The code is portable; it hasn't been run there yet. |
