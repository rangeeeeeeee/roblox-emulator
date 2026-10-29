use std::{env, path::PathBuf};

fn main() {
    let icon_dir=PathBuf::from("ui/studio-icons");
    println!("cargo:rerun-if-changed={}",icon_dir.display());
    let mut icons=std::fs::read_dir(&icon_dir).expect("Studio icons directory").filter_map(Result::ok)
        .filter(|entry|entry.file_type().is_ok_and(|kind|kind.is_file()))
        .filter_map(|entry|{
            let path=entry.path();let name=path.file_stem()?.to_str()?;
            if path.extension()?.to_str()? != "png" || name.is_empty() || name.len()>128
                || !name.bytes().all(|byte|byte.is_ascii_alphanumeric()||byte==b'_') {return None}
            Some(name.to_owned())
        }).collect::<Vec<_>>();
    icons.sort_unstable();
    let mut generated=String::from("fn studio_icon(name:&str)->Option<&'static [u8]>{match name{\n");
    for name in icons {generated.push_str(&format!("\"{name}\"=>Some(include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"),\"/ui/studio-icons/{name}.png\"))),\n"));}
    generated.push_str("_=>None}}\n");
    std::fs::write(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("studio_icons.rs"),generated).expect("Studio icon mapping");
    println!("cargo:rustc-check-cfg=cfg(emulator_local_tests)");
    let tests = ["diagnostics", "console", "lint", "executor", "instance", "model", "native", "scheduler", "types", "session", "web"]
        .map(|name| PathBuf::from(format!("local_tests/{name}.rs")));
    for path in &tests { println!("cargo:rerun-if-changed={}", path.display()); }
    let local_tests = tests.iter().all(|path| path.is_file());
    if local_tests { println!("cargo:rustc-cfg=emulator_local_tests"); }
    println!("cargo:rerun-if-changed=native/gc.cpp");
    println!("cargo:rerun-if-changed=native/filtergc.cpp");
    println!("cargo:rerun-if-changed=native/lint.cpp");
    println!("cargo:rerun-if-changed=native/bytecode.cpp");
    println!("cargo:rerun-if-changed=native/sandbox_windows.cpp");
    println!("cargo:rerun-if-changed=vendor/luau-lint-0.740/LinterAst.cpp");
    println!("cargo:rerun-if-changed=vendor/luau0-src-0.22.0/luau");
    let source = PathBuf::from("vendor/luau0-src-0.22.0/luau");
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17")
        .include(source.join("VM/include"))
        .include(source.join("VM/src"))
        .include(source.join("Common/include"))
        .include(source.join("Ast/include"))
        .include(source.join("Config/include"))
        .include(source.join("Inliner/include"))
        .define("LUA_API", "extern \"C\"")
        .define("LUAI_MAXCSTACK", "1000000")
        .define("LUA_VECTOR_SIZE", "3")
        .file("native/gc.cpp")
        .file("native/filtergc.cpp")
        .file("native/lint.cpp")
        .file("native/bytecode.cpp")
        .file("vendor/luau-lint-0.740/LinterAst.cpp");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        build.file("native/sandbox_windows.cpp");
        println!("cargo:rustc-link-lib=userenv");
        println!("cargo:rustc-link-lib=advapi32");
    }
    if local_tests { build.define("EMULATOR_LOCAL_TEST", None); }
    if env::var("DEBUG").as_deref() == Ok("true") {
        build.define("LUAU_ENABLE_ASSERT", None);
    }
    build.compile("emulator_native");
}
