use mlua::{Error, Result};

unsafe extern "C" {
    fn emu_prepare_bytecode(data: *mut u8, size: *mut usize, error: *mut u8, capacity: usize) -> i32;
}

/// Import compiler-produced Luau or Roblox opcode-encoded bytecode, not encrypted containers.
pub(crate) fn decode(source: &str) -> Result<(Vec<u8>, &'static str)> {
    let fail = |message: &str| Error::runtime(format!("Bytecode: {message}"));
    let input: Vec<u8> = source.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if input.is_empty() { return Err(fail("Base64 input is empty")); }
    let content = input.iter().position(|&b| b == b'=').unwrap_or(input.len());
    let padding = input.len() - content;
    if content % 4 == 1 || padding > 2 || input[content..].iter().any(|&b| b != b'=')
        || padding > 0 && (input.len() % 4 != 0 || padding != (4 - content % 4) % 4) {
        return Err(fail("Invalid Base64 padding"));
    }
    let mut bytes = Vec::with_capacity(content * 3 / 4);
    let (mut bits, mut accumulator) = (0, 0u32);
    for &b in &input[..content] {
        let value = match b {
            b'A'..=b'Z' => b - b'A', b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52, b'+' => 62, b'/' => 63,
            _ => return Err(fail("Invalid Base64 character; paste the encoded bytes without quotes")),
        };
        accumulator = (accumulator << 6) | u32::from(value); bits += 6;
        if bits >= 8 { bits -= 8; bytes.push((accumulator >> bits) as u8); }
        accumulator &= (1 << bits) - 1;
    }
    if accumulator != 0 { return Err(fail("Invalid Base64 trailing bits")); }
    let mut size = bytes.len(); let mut error = [0u8; 512];
    let encoding = unsafe { emu_prepare_bytecode(bytes.as_mut_ptr(), &mut size, error.as_mut_ptr(), error.len()) };
    if encoding == 0 {
        let end = error.iter().position(|&b| b == 0).unwrap_or(error.len());
        return Err(fail(&String::from_utf8_lossy(&error[..end])));
    }
    bytes.truncate(size);
    Ok((bytes, if encoding == 1 { "luau" } else { "roblox" }))
}
