# simpool

A pool of iOS simulators for people and coding agents who share one Mac.

Apple's Simulator is fast, but nothing manages it. Each script or agent creates
its own device, leaves it booted, and never cleans up. A few of those and the
Mac runs out of memory, disk, or processes, and every build on it starts
failing for reasons that look unrelated.

`simpool` gives each git worktree its own simulator, boots it in seconds from a
prepared template, and takes it back when the work is done.

```console
$ simpool lease
DF72C156-7D22-4762-BEC8-DFB7DFFFFDC8
simpool-2af373 (iPhone 17 Pro on iOS 26.5) is booted for `my-feature`, held for 8h

$ simpool install ios/build/Build/Products/Debug-iphonesimulator/MyApp.app
com.example.myapp installed on DF72C156-7D22-4762-BEC8-DFB7DFFFFDC8

$ simpool launch com.example.myapp
$ simpool tap 0.5 0.9
$ simpool type "hello@example.com"
$ simpool shot after.png
$ simpool release
simpool-2af373 shut down and returned to the pool
```

It is a thin, careful layer over `xcrun simctl`, plus the input that `simctl`
lacks. It is not an emulator and does not replace the Simulator.

## Install

Needs macOS with Xcode, and Rust 1.89 or newer.

```bash
cargo install --git https://github.com/williamrogersdev/simpool --tag v0.2.1
```

## Commands

| Command | What it does |
| --- | --- |
| `simpool lease` | Gets a booted simulator for this worktree. The same owner gets the same device every time. `--device "iPhone 17"`, `--runtime 26.5`, `--ttl 8h`. |
| `simpool release` | Shuts it down and returns it to the pool. `--delete` removes it. |
| `simpool reset` | Erases the leased simulator and boots it clean. |
| `simpool udid` | Prints the device id, for `xcrun simctl`, Xcode, Expo or Maestro. |
| `simpool install <app>` | Installs a simulator `.app` build. Refuses a device build and says why. |
| `simpool launch <bundle id>` | Launches the app, restarting it if it is running. |
| `simpool open-url <url>` | Opens a URL or deep link. |
| `simpool shot [path]` | Saves a screenshot and prints its path. |
| `simpool tap <x> <y>` | Taps the screen. `--hold 800` for a long press. |
| `simpool swipe <x> <y> <x2> <y2>` | Drags from one point to another. `--duration 300`. |
| `simpool type <text>` | Types into the focused field, as a hardware keyboard would. |
| `simpool key <name>` | Presses `return`, `delete`, `escape`, `tab`, `space` or an arrow. |
| `simpool button <name>` | Presses `home`, `lock`, `side` or `siri`. |
| `simpool view` | Serves a page that shows the screen live and sends your clicks, drags and keys to it. |
| `simpool mcp` | Runs as an MCP server, so a coding agent uses all of the above as tools. |
| `simpool status` | The pool, who holds what, what is booted, and disk used. |
| `simpool gc` | Plans a cleanup and lists hand-made simulators it will not touch. Changes nothing without `--apply`. |
| `simpool doctor` | Checks Xcode, the license, locale, process limit, runtimes and disk. |

Every command takes `--json`. The owner defaults to the name of the current git
worktree; set `--owner` or `SIMPOOL_OWNER` to choose another.

## What it protects you from

- **Hung commands.** Every call to Apple's tools has a timeout and an error
  that says what to run next.
- **Too many booted simulators.** `lease` refuses past `SIMPOOL_MAX_BOOTED`
  (default 4) and names what is booted.
- **Running out of processes.** A booted simulator runs about 200 processes.
  macOS limits processes per account, and going over that limit stops every
  shell on the machine from starting commands. `lease` checks the room first
  and keeps a margin (`SIMPOOL_PROCESS_MARGIN`, default 150).
- **Leftover devices.** A lease ends after its time without use, or when its
  worktree folder is deleted. `gc` returns those to the pool and deletes spare
  devices nobody has used for a day.
- **Other people's simulators.** `simpool` only ever shuts down or deletes
  devices it created. Its devices are named `simpool-…`.

## How it works

The first lease for a device type and iOS version creates a template device,
boots it once so iOS finishes its first-run setup, and shuts it down. Each new
pool device is a clone of that template, which is a copy-on-write copy on APFS,
so it is created in about a second and skips the slow first boot.

State is one JSON file in `~/Library/Application Support/simpool`, changed only
under a file lock, so many agents can call `simpool` at once.

## Touch and typing

Screen positions are fractions from the top left: `0.5 0.5` is the middle,
whatever the device. An agent reading a resized screenshot gets the same
answer as one reading the full image.

```console
$ simpool tap 0.85 0.49
$ simpool swipe 0.5 0.8 0.5 0.3
$ simpool type "Stand By?"
$ simpool key return
$ simpool button home
```

`simctl` has no input command, so these call the private SimulatorKit
framework inside Xcode, the same one the Simulator app uses. Tested on Xcode
27.2. If an Xcode release changes that framework, the commands say so and
everything else keeps working. `type` covers the US keyboard.

## Watch it live

```console
$ simpool view
http://127.0.0.1:52114/?k=5f0c…
```

Open the address in any browser. Click to tap, drag to swipe, type to type.
It refreshes about three times a second, listens on this Mac only, and
refuses any request without the key in the address.

## For coding agents (MCP)

`simpool mcp` speaks the Model Context Protocol on stdio, both the 2026-07-28
revision and the older handshake. The tools are `lease`, `release`, `reset`,
`status`, `doctor`, `install`, `launch`, `open_url`, `screenshot` (returns the
image), `tap`, `swipe`, `type_text`, `press_key` and `press_button`.

```json
{
  "mcpServers": {
    "simpool": { "command": "simpool", "args": ["mcp"] }
  }
}
```

The owner is the worktree the server starts in. When several agents share one
server, or the server does not start inside the agent's worktree, run
`simpool mcp --require-owner`: every call that touches a simulator must then
name its `owner`, so no two agents end up driving the same device.

## Not yet

- Pinch and other two-finger gestures.
- Typing characters outside the US keyboard.
- Video-rate streaming; the viewer sends screenshots.

## License

MIT
