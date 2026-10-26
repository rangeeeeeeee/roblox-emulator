use std::{env, fs, path::PathBuf};

fn luau_source() -> PathBuf {
    if let Some(path) = env::var_os("LUAU_SOURCE_DIR") {
        return PathBuf::from(path);
    }
    let cargo_home = env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).expect("CARGO_HOME is required")).join(".cargo")
    });
    let registry = cargo_home.join("registry/src");
    if let Ok(entries) = fs::read_dir(registry) {
        for entry in entries.flatten() {
            let path = entry.path().join("luau0-src-0.22.0+luau740/luau");
            if path.join("VM/src/lstate.h").is_file() {
                return path;
            }
        }
    }
    panic!("Luau740 sources not found; fetch pinned luau0-src 0.22.0 or set LUAU_SOURCE_DIR to its luau directory");
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(emulator_local_tests)");
    let tests = ["diagnostics", "console", "lint", "executor", "instance", "model", "native", "scheduler", "types", "session", "web"]
        .map(|name| PathBuf::from(format!("local_tests/{name}.rs")));
    for path in &tests { println!("cargo:rerun-if-changed={}", path.display()); }
    if tests.iter().all(|path| path.is_file()) { println!("cargo:rustc-cfg=emulator_local_tests"); }
    println!("cargo:rerun-if-changed=native/gc.cpp");
    println!("cargo:rerun-if-changed=native/filtergc.cpp");
    println!("cargo:rerun-if-changed=native/lint.cpp");
    println!("cargo:rerun-if-changed=vendor/luau-lint-0.740/LinterAst.cpp");
    println!("cargo:rerun-if-env-changed=LUAU_SOURCE_DIR");
    let source = luau_source();
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17")
        .include(source.join("VM/include"))
        .include(source.join("VM/src"))
        .include(source.join("Common/include"))
        .include(source.join("Ast/include"))
        .include(source.join("Config/include"))
        .define("LUA_API", "extern \"C\"")
        .define("LUAI_MAXCSTACK", "1000000")
        .define("LUA_VECTOR_SIZE", "3")
        .file("native/gc.cpp")
        .file("native/filtergc.cpp")
        .file("native/lint.cpp")
        .file("vendor/luau-lint-0.740/LinterAst.cpp");
    if env::var("DEBUG").as_deref() == Ok("true") {
        build.define("LUAU_ENABLE_ASSERT", None);
    }
    build.compile("emulator_native");
}
