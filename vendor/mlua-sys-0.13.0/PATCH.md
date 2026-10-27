Pinned upstream mlua-sys 0.13.0 with its Luau bytecode detection maximum synchronized to bundled Luau 0.740 `Bytecode.h` (14).
Required because upstream `LuauCompileFastpcall` emits version 14; the old detector incorrectly classified that bytecode as source text.
The compiler, interpreter, call accounting, stack limits, and opcodes are unchanged upstream code.
