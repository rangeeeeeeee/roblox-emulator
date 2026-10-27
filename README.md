# Roblox Emulator

A local Roblox/Luau research harness written in Rust and C++. Run server scripts, client scripts and executor commands against separate Luau VMs, inspect their state, and reproduce compatibility failures without launching a Roblox client.

This is a partial implementation of Roblox APIs, not a replacement for Studio or a guarantee that an anti-cheat behaves identically in Roblox. A passing scenario proves its assertions under the selected emulator profile.

## What is included

- Native DataModel objects, script environments, module loading and replication.
- Separate server, client and Actor VMs; tasks, signals, remotes, bindables and Actor messages.
- Executor APIs operating on real Luau objects in the selected VM.
- A local web console with server/client/executor contexts, Luau linting, completion, block indentation, Explorer and expandable output.
- A GC Viewer for inspecting live values and inserting retrieval expressions into the editor.
- JSON scenarios, bounded execution and machine-readable reports.

Private game sources, anti-cheat builds, captured profiles, local tests and prebuilt executables are deliberately excluded from this repository. The example below requires none of them.

## Build

Requirements: a current stable Rust toolchain supporting edition 2024, a C++17 toolchain, and Cargo access to the dependencies in `Cargo.lock`. The first build may download dependencies.

```sh
cargo build --release --locked
```

The executable is `target/release/roblox-emulator` (`.exe` on Windows). Keep one current runnable binary under that name; replace it when updating instead of keeping version-suffixed copies. Cargo's intermediate build files are not release artifacts.

The locally validated Windows configuration uses the `x86_64-pc-windows-gnu` Rust target and LLVM-MinGW. With both toolchains on `PATH`, its compiler settings are:

```powershell
$env:CC = 'x86_64-w64-mingw32-gcc.exe'
$env:CXX = 'x86_64-w64-mingw32-g++.exe'
$env:AR = 'llvm-ar.exe'
$env:CXXSTDLIB = 'c++'
$env:CXXFLAGS = '-include stdlib.h'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = $env:CC
cargo build --release --locked
```

For that toolchain, keep its `libc++.dll` and `libunwind.dll` beside the executable or on `PATH`. MSVC, Linux and macOS builds are not covered by the local validation reported here.

`build.rs` locates the pinned Luau sources in the Cargo registry. For a nonstandard source layout, set `LUAU_SOURCE_DIR` to the matching Luau directory containing `VM/`, `Common/`, `Ast/` and `Config/`. Updating Luau requires reviewing the C++ bridge and vendored patches together.

The editor bundle is committed and embedded in the executable. Node.js is only needed when changing the editor:

```sh
cd ui
npm ci
npm run build
cd ..
cargo build --release --locked
```

## Interactive use

From the repository root on Windows:

```powershell
.\target\release\roblox-emulator.exe --ui --port 0
```

Port `0` chooses an available loopback port. The process prints an `INTERPRETER_URL` containing the session token and opens it in the browser. Use that complete URL. Add `--no-browser` to open it manually. No arguments also start the UI.

Choose a realm and context before running a command. Client and executor commands in the same realm share a VM but use different environments. Another client or Actor has its own heap. The default session provides a server, a client and an Actor; custom scenarios define their own realms.

The UI starts paused. Use the running/stepping controls to advance scheduled work; reset recreates the session from the scenario file. Autoscroll is off by default.

To run compiled code, select **Bytecode (Base64)** above the editor, paste Base64 or open a `.b64` file, choose a VM/context and press **Ctrl+Enter**. Source remains the default. History preserves the input format. Bytecode uses the same environment, scheduler, output and execution budgets as source.

The importer accepts compiler-produced Luau bytecode versions 3–14, including Roblox opcode encoding (multiplier 227, decoded with 203) and its optional 24-byte trailer. It checks Base64, serialized boundaries, references and instruction operands before loading. This is not a complete hostile-bytecode verifier: use trusted compiler output. Encrypted/compressed containers, class constants and runtime-specialized userdata opcodes are rejected. A script still needs its original game objects/modules; successful loading does not provide those dependencies.

For `/api/execute`, pass `{"realm":"client1","context":"client","format":"bytecode-base64","source":"<Base64>"}`. Omit `format` or use `"source"` for Luau text. `loadstring` remains source-only.

In an **Executor** context:

```lua
print(identifyexecutor())
print("GC objects", #getgc(true))
Instance.new("GCViewer")
```

GC Viewer uses weak object handles. Inserted `getgcvalue(viewer, snapshot, index)` expressions belong to that realm and snapshot; they expire after refresh, close, reset or ten minutes. Strings are copied into bounded host storage because Luau strings do not behave as weak references.

## A self-contained scenario

Save this as `scenario.json`. It registers a server listener, sends one client packet and requires the server's output.

```json
{
  "name": "Remote delivery",
  "engine": "native",
  "duration": 1,
  "step": 0.016666666666666666,
  "nodes": [
    {"id": "player", "class": "Player", "name": "TestPlayer", "parent": "Players", "properties": {"UserId": 1}},
    {"id": "remote", "class": "RemoteEvent", "name": "Probe", "parent": "ReplicatedStorage"},
    {"id": "serverScript", "class": "Script", "name": "Receiver", "parent": "ServerScriptService", "source": "game.ReplicatedStorage.Probe.OnServerEvent:Connect(function(player, value) assert(value == 42); print('PACKET_OK', player.Name) end)"},
    {"id": "clientScript", "class": "LocalScript", "name": "Sender", "parent": "ReplicatedFirst", "source": "task.wait(0.1); game.ReplicatedStorage.Probe:FireServer(42)"}
  ],
  "realms": [
    {"id": "server", "side": "server", "entry": ["serverScript"]},
    {"id": "client1", "side": "client", "clientId": "player", "executor": true, "entry": ["clientScript"]}
  ],
  "expect": ["PACKET_OK"],
  "reject": []
}
```

```powershell
.\target\release\roblox-emulator.exe .\scenario.json --report .\report.json
.\target\release\roblox-emulator.exe --ui .\scenario.json --port 0
```

Use `sourcePath` instead of `source` to load a Luau file relative to the scenario's directory. Node `id` values identify objects; `parent`, realm `entry`, `clientId` and `actorId` refer to those IDs. Standard services such as `Players` and `ReplicatedStorage` are provided by the runtime. Set `actorId` on a client realm to associate it with an Actor node. `executorEntry` lists scripts to run in an enabled executor context.

| Setting | Meaning |
| --- | --- |
| `duration`, `step`, `latency` | Simulated seconds, scheduler step and remote delivery delay. |
| `signalBehavior` | `deferred` by default, or `immediate`. |
| `memoryMb`, `cpuMs` | Per-VM memory limit and wall-time execution budget. |
| `traceErrors` | Observe protected-call errors where Luau exposes a debug callback. |
| `expect`, `reject` | Required and forbidden output substrings across the entire run. |
| `expectedErrors` | Allowances with `realm`, exact `message` and `maxCount`; errors remain in the report. |
| Realm `executorProfile` | `generic` or the measured, version-specific `macsploit-744.2` profile. |

Batch exit status is `0` for a passing run and `1` for failure. Unexpected script errors, missing expectations, rejected output, unsupported API observations and exceeded limits can fail a run. Invalid CLI arguments may exit with `2`. Reports include source hashes, logs and diagnostic data; retain the exact scenario used to reproduce a result.

## Compatibility boundaries

| Area | Current boundary |
| --- | --- |
| Luau | Pinned Luau 0.740 through `luau0-src` 0.22.0; real VM closures, stack frames and heap objects. Roblox builds may differ. |
| Actors | Independent VMs with modeled serial/parallel restrictions; sequential scheduling, not OS-level parallel races. |
| Time | Scheduler time is simulated. `os.clock()` measures elapsed host time; simulated RenderStepped rate is not rendering performance. |
| DataModel | Implemented APIs plus a class/enum catalog. Catalog membership does not imply complete member implementation. |
| Physics and animation | Selected spatial queries and animation/character contracts; no complete physics solver, renderer or asset playback. |
| Network | In-process routing and configured latency, not Roblox's transport stack or live services. |
| GC | Current-VM enumeration; specific host bookkeeping objects are excluded. Counts and collection timing need independent comparison with Roblox. |
| Executor | A defined local API profile, not universal executor compatibility or a full sUNC pass. |

Implemented executor families include `getgc`, `filtergc`, `getgenv`, `getrenv`, `getsenv`, `getconnections`, closure classification, `newcclosure`, `clonefunction`, `hookfunction`, `hookmetamethod`, restoration, raw metatables, readonly state, upvalues, constants and interpreted stack access. See [src/executor.rs](src/executor.rs) and [native/gc.cpp](native/gc.cpp) for restrictions.

`filtergc` hashes and inactive prototypes are unsupported. `getproto` supports activated closures; `getprotos` is unsupported. Hooks can reject active frames or incompatible closure layouts. Host-internal functions cannot be mutated. File and clipboard APIs use bounded virtual storage. HTTP, process control and several protected engine APIs are unsupported. There is no `.rbxl` importer; define the DataModel in JSON.

The web server binds to `127.0.0.1` and requires a per-session token. This is a local development tool, not an isolation boundary for arbitrary hostile code: the native bridge uses Luau VM internals.

## Research workflow

1. Reduce the observation to a small scenario with an explicit expected result.
2. Separate engine behavior, executor behavior and emulator-only helpers. Record the Roblox build, executor version, VM context and signal mode for reference captures.
3. Compare return values, errors, stack frames, identities, yielding and cleanup. A missing trace does not prove that no error occurred.
4. Include an unmodified control and a deliberately changed case. Repeat timing-sensitive tests across GC cycles and scheduler steps.
5. Report a mismatch with the scenario, source hashes, selected profile and output. Do not replace unsupported behavior with success to make a detector pass.

For example, a weak-table sentinel disappearing does not prove another object should disappear in the same GC cycle. Ordinary code touching that object can affect the result. Distinguish a temporary observation from a persistent external reference.

The CLI also accepts `--native-probe <probe.luau> <reference.json>` and `--runtime-probe <probe.luau> <reference.json>` for comparisons against your own reference captures. Private probes and datasets are not distributed here. The excluded `local_tests/` suite is enabled only when its expected files are present; `cargo test` on a public checkout does not establish that this private suite ran.

## Code map

| Path | Responsibility |
| --- | --- |
| `src/main.rs`, `src/session.rs`, `src/web.rs` | Scenario host, interactive execution and local HTTP interface. |
| `src/instance.rs`, `src/model.rs` | Object identities, DataModel behavior, environments and replication. |
| `src/scheduler.rs` | Tasks, signals, connections and execution context. |
| `src/types.rs`, `src/spatial.rs`, `src/animation.rs` | Datatypes and selected spatial/animation behavior. |
| `src/executor.rs`, `src/native.rs`, `native/` | Executor surface, C++ VM access and Luau linting. |
| `src/console.rs`, `src/inspector.rs`, `src/diagnostics.rs` | Output snapshots, live inspection and error observations. |
| `ui/` | CodeMirror editor and browser interface; bundled into the Rust binary. |
| `runtime/` | Earlier Luau behavioral backend, explicitly selected with `engine: "behavioral"` in batch scenarios. |
| `vendor/` | Pinned dependencies with required local patches. |

Keep changes scoped to a reproducible behavior. When editing the VM bridge, verify successful calls and error/yield paths, preserving closure identity and VM ownership. Keep private scripts, traces, keys, binaries and machine-specific launch configuration out of commits; `.gitignore` uses an allowlist for this purpose.

Vendored changes are documented in [mlua/PATCH.md](vendor/mlua-0.12.2/PATCH.md) and [mlua-sys/PATCH.md](vendor/mlua-sys-0.13.0/PATCH.md). Their upstream licenses are retained; the editor bundle includes dependency notices. No top-level license for the emulator's own code has been declared in this checkout.
