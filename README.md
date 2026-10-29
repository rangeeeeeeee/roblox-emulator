# Roblox Emulator

A local Roblox/Luau research harness written in Rust and C++. Run server scripts, client scripts and executor commands against separate Luau VMs, inspect their state, and reproduce compatibility failures without launching a Roblox client.

This is a partial implementation of Roblox APIs, not a replacement for Studio or a guarantee that an anti-cheat behaves identically in Roblox. A passing scenario proves its assertions under the selected emulator profile.

## Executor compatibility results

Measured on Windows on 29 October 2026 in the default headless session, without additional game scripts or modules. These numbers are results of the named tests, not overall API coverage or Roblox equivalence.

| Test | Profile | Result |
| --- | --- | --- |
| [Archived UNC Environment Check](https://github.com/unified-naming-convention/NamingStandard/blob/main/UNCCheckEnv.lua) | `generic` | 72/83 passed (87%); 6 missing alias groups |
| Archived UNC Environment Check | `unc` | 74/83 passed (89%); 6 missing alias groups |
| [Vyperia UNC](https://gitlab.com/ltseverydayyou/uuuuuuu/-/raw/main/UNC%20test) | `generic` | 105/161 scored functions passed (65.2%); 7 untested excluded |
| Vyperia UNC | `unc` | 106/161 scored functions passed (65.8%); 7 untested excluded |
| [sUNC](https://docs.sunc.io/) | `generic` | No score: the default 64 MiB VM exhausted memory; with 512 MiB and a 10 s execution budget, execution exceeded the budget. |

The UNC test expects Roblox game scripts and modules absent from the default session, and its `ws://echo.websocket.events` endpoint did not complete a handshake. Vyperia's approved HTTPS and WSS echo checks passed. GitLab returned HTTP 429 during the Vyperia run, so the run used a previously downloaded, byte-for-byte verified copy (SHA-256 `f35567a617eef539576c026b6d968ce091a98b46dbd4f0f8ca94f49abc841808`). The sUNC project states that v2 requires its dedicated testing game for verified results; no official sUNC score is claimed here.

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
$env:CXXSTDLIB = 'static=c++'
$env:CXXFLAGS = '-include stdlib.h'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = $env:CC
$env:CARGO_ENCODED_RUSTFLAGS = '-Lnative=C:\path\to\llvm-mingw\x86_64-w64-mingw32\lib'
cargo build --release --locked
```

Replace the example library path with your LLVM-MinGW installation. Static C++ runtime linkage is required for the restricted worker to start without granting access to user directories. MSVC builds are not covered by the local validation reported here; execution on non-Windows systems fails closed because no equivalent OS sandbox is implemented.

The pinned Luau sources are vendored so the C++ bridge and VM use the same patched version. Updating Luau requires reviewing the bridge and inactive-prototype call guard together. Native Luau code generation is disabled because it would bypass that guard.

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

To send keyboard input, select a client under **Input client**, click **Game input**, and press the desired keys. Press Escape to leave input mode; typing in the code editor is not sent to the game. **Executor files** lists the selected VM's virtual workspace and can download a file; that workspace is cleared on reset.

The UI starts paused. Use the running/stepping controls to advance scheduled work; reset recreates the session from the scenario file. Autoscroll is off by default.

For an editable test place, switch Explorer to **Place**, select a parent such as `ServerScriptService` or `StarterPlayerScripts`, and create a `Script`, `LocalScript`, `ModuleScript`, `Actor`, or another supported instance. Drag instances in Explorer to reparent them and open a script in its own editor tab. **Save Place** downloads a JSON file with the draft, sources and selected executor profile; **Open Place** imports it. **Run Place** starts fresh server/client and applicable Actor VMs from the draft. Supported scenarios opened with `--ui` are imported into the draft with their realm and Actor configuration. If import fails, Explorer shows the reason and the original runtime remains available. The initial character is headless and has no physics or full R15 rig. Editing a draft does not rewrite the original scenario.

Explorer switches to **Runtime** when a test starts and returns to **Place** when it stops. In the selected VM, create, rename, move, duplicate or delete supported instances, drag them in the tree, and edit supported properties. Runtime edits do not change the saved place. New runtime scripts are empty and do not start automatically. Auto-refresh is optional; double-click an available script to open a read-only source snapshot. Runtime snapshots close on reset or when running a place.

Edits are debounced into the place draft and a recovery copy beside the executable in `autosaves/`. Use **File → Open Auto Save** to choose a copy after a restart; it never overwrites an imported place file. Recovery files are local plaintext JSON containing the place's scripts and are excluded from the repository. Export a place explicitly with **Save Place**. The Output options include an opt-in view of caught errors; those diagnostic entries are kept in host output and are not sent to `LogService`.

Interactive Windows sessions support executor `request`/`http_request`/`http.request`/`syn.request`, executor `game:HttpGet` and `httpget`, and server `HttpService:RequestAsync`, `GetAsync` and `PostAsync`. Every request stops the VMs and shows its method, URL, headers and complete body for individual approval. Rejecting it raises a Lua error; closing or resetting the session never approves it. The executor's default User-Agent is `Roblox/WinInet`; `HttpService` uses `RobloxStudio/WinInet` and locks that header. Explicit executor headers are retained. Redirects are not followed automatically, and an offline batch run cannot make HTTP requests.

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

Use `sourcePath` instead of `source` to load a Luau file; relative paths resolve from the scenario's directory. CLI scenario files can also reference absolute paths or paths outside that directory, so open only trusted scenarios. The editable Place importer rejects these path fields. Node `id` values identify objects; `parent`, realm `entry`, `clientId` and `actorId` refer to those IDs. Standard services such as `Players` and `ReplicatedStorage` are provided by the runtime. Set `actorId` on a client realm to associate it with an Actor node. `executorEntry` lists scripts to run in an enabled executor context.

| Setting | Meaning |
| --- | --- |
| `duration`, `step`, `latency` | Simulated seconds, scheduler step and remote delivery delay. |
| `signalBehavior` | `deferred` by default, or `immediate`. |
| `memoryMb`, `cpuMs` | Per-VM memory limit and wall-time execution budget. |
| `traceErrors` | Observe protected-call errors where Luau exposes a debug callback. |
| `expect`, `reject` | Required and forbidden output substrings across the entire run. |
| `expectedErrors` | Allowances with `realm`, exact `message` and `maxCount`; errors remain in the report. |
| Realm `executorProfile` | `generic`, `unc`, `macsploit-744.2`, or `macsploit-744.4`. The `unc` profile returns `loadfile` syntax errors as `nil, error` and compiles executor source with Luau debug level 2, preserving local slots at the cost of extra debug metadata. Generic throws `loadfile` syntax errors as specified by sUNC. The 744.4 profile reports the simulated executor name and inherits the 744.2 GC model; its GC behavior was not measured in 744.4. |

Batch exit status is `0` for a passing run and `1` for failure. Unexpected script errors, missing expectations, rejected output, unsupported API observations and exceeded limits can fail a run. Invalid CLI arguments may exit with `2`. Reports include source hashes, logs and diagnostic data; retain the exact scenario used to reproduce a result.

## Compatibility boundaries

| Area | Current boundary |
| --- | --- |
| Luau | Pinned Luau 0.740 through `luau0-src` 0.22.0; real VM closures, stack frames and heap objects. Roblox builds may differ. |
| Actors | Independent VMs with modeled serial/parallel restrictions; sequential scheduling, not OS-level parallel races. |
| Time | Scheduler time is simulated. `os.clock()` measures elapsed host time; simulated RenderStepped rate is not rendering performance. |
| DataModel | Implemented APIs plus a class/enum catalog. Catalog membership does not imply complete member implementation. |
| Physics and animation | Selected spatial queries and animation/character contracts; no complete physics solver, renderer or asset playback. |
| Network | In-process remote routing plus individually approved Windows HTTP(S); not Roblox's network transport. |
| GC | Current-VM enumeration; specific host bookkeeping objects are excluded. Counts and collection timing need independent comparison with Roblox. |
| Executor | A defined local API profile, not universal executor compatibility or a full sUNC pass. |

Implemented executor families include `getgc`, `filtergc`, per-VM Instance cache operations, `getgenv`, `getrenv`, script and callback inspection, `getconnections`, closure classification, `newcclosure`, `clonefunction`, `hookfunction`, `hookmetamethod`, restoration, raw metatables, readonly state, upvalues, constants, interpreted stack access, binary-safe Base64/LZ4, hashes and AES-256-CBC encryption. See [src/executor.rs](src/executor.rs) and [native/gc.cpp](native/gc.cpp) for restrictions.

`filtergc` hashes are unsupported. `getproto` and `getprotos` return inspect-only inactive Luau closures that return no values without executing their prototype, as observed in the MacSploit probe; `getproto(..., true)` enumerates active closures. `getfunctionhash` hashes normalized Luau instructions and primitive constants, and its bytes have not been verified against a real executor. Hooks can reject active frames or incompatible closure layouts. Host-internal functions cannot be mutated. Script bytecode is recompiled from the emulator's source and does not reproduce Roblox's bytecode packaging. File and clipboard APIs use bounded virtual storage; `getcustomasset` stores virtual file bytes in the selected VM. `ContentProvider:PreloadAsync` validates PCM WAV data referenced by a Sound; audio playback and other formats are unsupported. Drawing objects have bounded, typed headless state, but no renderer or `TextBounds`. `setfpscap` limits the selected VM's simulated render events, not an actual renderer. `getsimulationradius` and `setsimulationradius` expose local Player properties but do not model physics or ownership. `debug.setinfo` changes Lua prototype metadata, not the instruction lines of an active frame. `loadfile` throws syntax errors in the generic profile as specified by sUNC; the 744.4 profile returns `nil, error` as observed in MacSploit. HTTP requires Windows and interactive approval; unsafe nested call boundaries, gzip request compression and custom `RequestAsync` timeouts are unsupported. Approval replaces Studio's HTTP-enabled setting; Roblox rate limits and domain restrictions are not modeled. WebSocket connections also require individual approval and use a separate bounded broker; messages on an approved connection do not prompt again. Thread identity is modeled per coroutine, with Source access checked for identities 2 and 8; other Roblox security capabilities are not modeled. AES-256-CBC uses the first block of a 32-byte IV, as observed in MacSploit 744.4; a supplied IV returns one result in that profile. Encryption modes other than AES-256-CBC and several protected engine APIs are unsupported. There is no `.rbxl` importer; create an editable place or define a scenario in JSON.

`getscriptfromthread` currently infers ownership from the thread's environment, so replacing the environment can change the result; it does not provide the specified independent weak script reference. `getscriptthread` tracks an explicitly launched live root thread, not ModuleScript threads created by `require` or child threads.

The executor `Regex` API uses a bounded byte-oriented Rust engine; lookaround and backreferences are unsupported, and a single search is not preempted by the Luau timer. This differs from the separate [Roblox luau-regexp module](https://github.com/Roblox/luau-regexp), whose public API uses `exec` and `test`.

The web server binds to `127.0.0.1` and requires a per-session token. On Windows, scripts, bytecode, the Luau parser and all VMs run in a separate Less Privileged AppContainer worker without network capabilities. A Job Object limits the worker to one process and 8 GiB; the host communicates through bounded pipes and fails closed if isolation cannot be established. HTTP requests are validated and sent by the host only after approval. Executor file and clipboard APIs remain virtual.

The server, client and Actor VMs share that worker process. A native compromise inside the worker can therefore expose another VM in the same session, and AppContainer does not protect against Windows vulnerabilities or accesses explicitly allowed by the OS. The worker's own AppContainer storage is not subject to a disk quota. Use a separate virtual machine for stronger isolation. Scenario `sourcePath` and insertion files must resolve within the scenario directory, or its parent when the directory is named `scenarios`; total assembled source is limited to 32 MiB. Select scenarios from trusted projects, since those allowed source files are read by the host before worker startup.

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

Vendored changes are documented in [mlua/PATCH.md](vendor/mlua-0.12.2/PATCH.md), [mlua-sys/PATCH.md](vendor/mlua-sys-0.13.0/PATCH.md), and [luau0-src/PATCH.md](vendor/luau0-src-0.22.0/PATCH.md). Their upstream licenses are retained; the editor bundle includes dependency notices. No top-level license for the emulator's own code has been declared in this checkout.
