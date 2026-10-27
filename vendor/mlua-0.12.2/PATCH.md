Pinned upstream mlua 0.12.2 with scoped native callback and protected-call extensions.
`Error::lua_string` marks only intentional native engine errors for a real Lua string throw.
The callback boundary pushes the string under protection after Rust guards drop, then throws in the original C frame.
All other Rust errors/panics and Lua error identities keep upstream handling; no debug/stack results are rewritten.
`Lua::invoke_native_callback` reuses the same guarded boundary for native return/yield/original-value error actions, performed only after Rust guards and the reserved error slot are released.
Its `Dispatch` action forwards untouched arguments to a selected native C entry after guards drop, preserving the original VM frame and closure upvalues.
Luau protected calls reuse two stateless C entries on the private reference thread instead of allocating two closures per call; the same pcall/error frames and allocator accounting remain real.
A thread-local call token guards the dispatcher, completed results are checked, and host entry pointers are registered so the executor rejects their mutation with `HostInternalClosure`; actual GC enumeration still includes them.
The Rust-only `is_reference_thread` predicate identifies mlua's auxiliary handle-storage thread so direct executor thread enumeration omits it; ordinary Lua threads remain visible.
