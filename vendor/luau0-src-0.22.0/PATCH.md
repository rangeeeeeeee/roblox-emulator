# Luau 0.740 patches

Pinned from `luau0-src` 0.22.0. An inspection-only closure uses `preload = 2`; VM call, protected-call and metamethod entry points return without executing its unbound prototype. This matches a MacSploit probe returning nil, although its exact return count was not measured. Other unbound closures remain rejected. Cloning preserves the marker. The emulator bridge prevents hooks from replacing or activating these closures.

Native code generation is disabled: generated code would need the same call guard. Recheck every entry path, clone and hook when updating Luau. The bundled source tree excludes CodeGen, which the emulator does not use.

The optional host diagnostics observer also sees Lua pcall/xpcall errors across non-yieldable C boundaries. It runs before the xpcall error handler, observes the original error without changing the Lua stack, and does not call Luau's debugger callback in those boundaries. Host observation is disabled unless the emulator installs it.
