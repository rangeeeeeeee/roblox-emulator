use mlua::{Error, Function, Lua, LuaString, MultiValue, Result, Table, Value};
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha2::Digest;
use std::ffi::CString;

// Bound host allocations independently of Luau's VM memory accounting.
const MAX_DATA: usize = 8 * 1024 * 1024;
const MAX_BASE64: usize = MAX_DATA.div_ceil(3) * 4;

fn bounded(size: usize) -> Result<()> {
    if size > MAX_DATA { return Err(Error::lua_string("BudgetExceeded: encoding data size")); }
    Ok(())
}

fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    bounded(bytes.len())?;
    Ok(lz4_flex::block::compress(bytes))
}

fn decompress(bytes: &[u8], expected: Option<usize>) -> Result<Vec<u8>> {
    if bytes.len() > lz4_flex::block::get_maximum_output_size(MAX_DATA) {
        return Err(Error::lua_string("BudgetExceeded: encoding data size"));
    }
    if let Some(size) = expected {
        bounded(size)?;
        return lz4_flex::block::decompress(bytes, size)
            .map_err(|err| Error::lua_string(format!("invalid LZ4 block: {err}")));
    }
    let mut size = bytes.len().saturating_mul(4).clamp(1, MAX_DATA);
    loop {
        match lz4_flex::block::decompress(bytes, size) {
            Ok(data) => return Ok(data),
            Err(lz4_flex::block::DecompressError::OutputTooSmall { expected, .. }) => {
                if size >= MAX_DATA || expected > MAX_DATA {
                    return Err(Error::lua_string("BudgetExceeded: LZ4 output size"));
                }
                size = size.saturating_mul(2).max(expected).min(MAX_DATA);
            },
            Err(err) => return Err(Error::lua_string(format!("invalid LZ4 block: {err}"))),
        }
    }
}

fn hash(bytes: &[u8], algorithm: &str) -> Result<String> {
    bounded(bytes.len())?;
    let digest = match algorithm.to_ascii_lowercase().as_str() {
        "sha1" | "sha-1" => sha1::Sha1::digest(bytes).to_vec(),
        "sha256" | "sha-256" => sha2::Sha256::digest(bytes).to_vec(),
        "sha384" | "sha-384" => sha2::Sha384::digest(bytes).to_vec(),
        "sha512" | "sha-512" => sha2::Sha512::digest(bytes).to_vec(),
        "sha3-224" => sha3::Sha3_224::digest(bytes).to_vec(),
        "sha3-256" => sha3::Sha3_256::digest(bytes).to_vec(),
        "sha3-384" => sha3::Sha3_384::digest(bytes).to_vec(),
        "sha3-512" => sha3::Sha3_512::digest(bytes).to_vec(),
        "md5" | "md-5" => md5::Md5::digest(bytes).to_vec(),
        _ => return Err(Error::lua_string("unsupported hash algorithm")),
    };
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn hmac(key: &[u8], bytes: &[u8], algorithm: &str) -> Result<String> {
    bounded(key.len())?;
    bounded(bytes.len())?;
    macro_rules! digest { ($algorithm:ty) => {{
        let mut mac = <Hmac<$algorithm> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(bytes);
        mac.finalize().into_bytes().to_vec()
    }}; }
    let digest = match algorithm.to_ascii_lowercase().as_str() {
        "sha1" | "sha-1" => digest!(sha1::Sha1),
        "sha256" | "sha-256" => digest!(sha2::Sha256),
        "sha384" | "sha-384" => digest!(sha2::Sha384),
        "sha512" | "sha-512" => digest!(sha2::Sha512),
        "sha3-224" => digest!(sha3::Sha3_224),
        "sha3-256" => digest!(sha3::Sha3_256),
        "sha3-384" => digest!(sha3::Sha3_384),
        "sha3-512" => digest!(sha3::Sha3_512),
        "md5" | "md-5" => digest!(md5::Md5),
        _ => return Err(Error::lua_string("unsupported HMAC algorithm")),
    };
    Ok(base64::encode(digest))
}

fn decode_crypto(value: &LuaString, label: &str) -> Result<Vec<u8>> {
    if value.as_bytes().len() > MAX_BASE64 { return Err(Error::lua_string("BudgetExceeded: encoding data size")); }
    let decoded = base64::decode(value.as_bytes().as_ref())
        .map_err(|_| Error::lua_string(format!("invalid Base64 {label}")))?;
    bounded(decoded.len())?;
    Ok(decoded)
}

fn aes_mode(mode: Option<String>) -> Result<()> {
    if mode.as_deref().unwrap_or("CBC").eq_ignore_ascii_case("CBC") { Ok(()) }
    else { Err(Error::lua_string("UnsupportedApi: crypt AES mode (only CBC is implemented)")) }
}

fn aes_key_iv(key: &LuaString, iv: &LuaString) -> Result<(Vec<u8>, Vec<u8>)> {
    let key = decode_crypto(key, "key")?;
    let mut iv = decode_crypto(iv, "IV")?;
    if key.len() != 32 { return Err(Error::lua_string("AES-256 key must contain 32 bytes")); }
    if !matches!(iv.len(),16|32) { return Err(Error::lua_string("AES-CBC IV must contain 16 or 32 bytes")); }
    // MacSploit 744.4 uses the first block of an explicitly supplied 32-byte IV.
    iv.truncate(16);
    Ok((key, iv))
}

fn aes_encrypt(lua: &Lua, data: LuaString, key: LuaString, iv: Option<LuaString>, mode: Option<String>, mac_744_4: bool) -> Result<MultiValue> {
    aes_mode(mode)?;
    let supplied_iv = iv.is_some();
    let iv = match iv {
        Some(iv) => iv,
        None => { let mut bytes = [0u8; 16]; getrandom::fill(&mut bytes).map_err(Error::external)?; lua.create_string(base64::encode(bytes))? }
    };
    let (key, iv_bytes) = aes_key_iv(&key, &iv)?;
    let data = data.as_bytes();
    bounded(data.len())?;
    // PKCS#7 adds a full block to aligned plaintext. Bound the padded output too.
    let padded = data.len().checked_add(16 - data.len() % 16).ok_or_else(||Error::lua_string("BudgetExceeded: encoding data size"))?;
    bounded(padded)?;
    let mut buffer = vec![0; padded];
    buffer[..data.len()].copy_from_slice(&data);
    let encryptor = cbc::Encryptor::<aes::Aes256>::new_from_slices(&key, &iv_bytes).map_err(|_| Error::lua_string("invalid AES key or IV"))?;
    let ciphertext = encryptor.encrypt_padded_mut::<Pkcs7>(&mut buffer, data.len()).map_err(|_| Error::lua_string("AES encryption failed"))?;
    let mut values = vec![Value::String(lua.create_string(base64::encode(ciphertext))?)];
    if !mac_744_4 || !supplied_iv { values.push(Value::String(iv)); }
    Ok(MultiValue::from_vec(values))
}

fn aes_decrypt(lua: &Lua, data: LuaString, key: LuaString, iv: LuaString, mode: Option<String>) -> Result<LuaString> {
    aes_mode(mode)?;
    let (key, iv) = aes_key_iv(&key, &iv)?;
    let mut bytes = decode_crypto(&data, "ciphertext")?;
    let decryptor = cbc::Decryptor::<aes::Aes256>::new_from_slices(&key, &iv).map_err(|_| Error::lua_string("invalid AES key or IV"))?;
    let plaintext = decryptor.decrypt_padded_mut::<Pkcs7>(&mut bytes).map_err(|_| Error::lua_string("invalid AES ciphertext or padding"))?;
    lua.create_string(plaintext)
}

fn named<A: mlua::FromLuaMulti, R: mlua::IntoLuaMulti>(
    lua: &Lua,
    env: &Table,
    name: &str,
    callback: impl Fn(&Lua, A) -> Result<R> + 'static,
) -> Result<Function> {
    let function = lua.create_function(callback)?;
    crate::native::name_function(lua, &function, &CString::new(name).unwrap())?;
    crate::native::mark_executor_function(lua, &function, env)?;
    Ok(function)
}

pub fn install(lua: &Lua, env: &Table, caps: &crate::native::Capabilities) -> Result<()> {
    let mac_744_4 = crate::native::is_macsploit_7444(lua);
    let encode = named(lua, env, "base64encode", |lua, data: LuaString| {
        bounded(data.as_bytes().len())?;
        lua.create_string(base64::encode(data.as_bytes().as_ref()))
    })?;
    let decode = named(lua, env, "base64decode", move |lua, data: LuaString| {
        if data.as_bytes().len() > MAX_BASE64 {
            return Err(Error::lua_string("BudgetExceeded: encoding data size"));
        }
        let input = data.as_bytes();
        let filtered;
        let bytes = if mac_744_4 {
            filtered = input.iter().copied().filter(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=')).collect::<Vec<_>>();
            if filtered.len() % 4 != 0 { return Err(Error::lua_string("vector")); }
            filtered.as_slice()
        } else { input.as_ref() };
        let decoded = base64::decode(bytes)
            .map_err(|err| Error::lua_string(format!("invalid Base64: {err}")))?;
        bounded(decoded.len())?;
        lua.create_string(decoded)
    })?;
    let base64_table = lua.create_table()?;
    base64_table.set("encode", encode.clone())?;
    base64_table.set("decode", decode.clone())?;
    env.set("base64", base64_table.clone())?;
    for name in ["base64encode", "base64_encode"] { env.set(name, encode.clone())?; }
    for name in ["base64decode", "base64_decode"] { env.set(name, decode.clone())?; }

    let crypt = lua.create_table()?;
    crypt.set("base64", base64_table)?;
    for name in ["base64encode", "base64_encode"] { crypt.set(name, encode.clone())?; }
    for name in ["base64decode", "base64_decode"] { crypt.set(name, decode.clone())?; }
    crypt.set("hash", named(lua, env, "hash", |_, (data, algorithm): (LuaString, String)| {
        hash(data.as_bytes().as_ref(), &algorithm)
    })?)?;
    crypt.set("hmac", named(lua, env, "hmac", |_, (key, data, algorithm): (LuaString, LuaString, String)| {
        hmac(key.as_bytes().as_ref(), data.as_bytes().as_ref(), &algorithm)
    })?)?;
    crypt.set("encrypt", named(lua, env, "encrypt", move |lua, (data, key, iv, mode): (LuaString, LuaString, Option<LuaString>, Option<String>)| {
        aes_encrypt(lua, data, key, iv, mode, mac_744_4)
    })?)?;
    crypt.set("decrypt", named(lua, env, "decrypt", |lua, (data, key, iv, mode): (LuaString, LuaString, LuaString, Option<String>)| {
        aes_decrypt(lua, data, key, iv, mode)
    })?)?;
    crypt.set("random", named(lua, env, "random", |lua, count: usize| {
        bounded(count)?;
        let mut bytes = vec![0; count];
        getrandom::fill(&mut bytes).map_err(Error::external)?;
        lua.create_string(bytes)
    })?)?;
    crypt.set("generatebytes", named(lua, env, "generatebytes", |_, count: usize| {
        bounded(count)?;
        let mut bytes = vec![0; count];
        getrandom::fill(&mut bytes).map_err(Error::external)?;
        Ok(base64::encode(bytes))
    })?)?;
    crypt.set("generatekey", named(lua, env, "generatekey", |_, ()| {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(Error::external)?;
        Ok(base64::encode(key))
    })?)?;
    env.set("crypt", crypt)?;

    env.set("lz4compress", named(lua, env, "lz4compress", |lua, data: LuaString| {
        lua.create_string(compress(data.as_bytes().as_ref())?)
    })?)?;
    env.set("lz4decompress", named(lua, env, "lz4decompress", move |lua, (data, size): (LuaString, Option<usize>)| {
        if mac_744_4 && size.is_none() { return Err(Error::lua_string("missing argument #2")); }
        lua.create_string(decompress(data.as_bytes().as_ref(), size)?)
    })?)?;
    for name in ["base64encode", "base64decode", "lz4compress", "lz4decompress", "crypt.hash", "crypt.generatebytes", "crypt.generatekey"] {
        caps.set(name, "native: bounded binary-safe encoding")?;
    }
    caps.set("crypt.hmac", "native: bounded binary-safe HMAC; Base64 result")?;
    caps.set("crypt.random", "native: bounded random bytes from the operating system")?;
    for name in ["crypt.encrypt", "crypt.decrypt"] { caps.set(name, "partial: AES-256-CBC/PKCS7; first block of 32-byte IV is an unverified assumption; other modes unsupported")?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz4_roundtrips_and_rejects_bad_input() {
        for data in [&b""[..], &b"Hello, world! Hello, world!"[..], &[0, 255, 0, 1][..], &vec![42; 1024][..]] {
            let packed = compress(data).unwrap();
            assert_eq!(decompress(&packed, Some(data.len())).unwrap(), data);
            assert_eq!(decompress(&packed, None).unwrap(), data);
        }
        assert_eq!(decompress(b"\x50hello", Some(5)).unwrap(), b"hello");
        assert!(compress(&vec![42; 1024]).unwrap().len() < 1024);
        assert!(decompress(&[0xff], None).is_err());
        assert!(decompress(&[0], Some(MAX_DATA + 1)).is_err());
    }

    #[test]
    fn base64_handles_binary_and_rejects_invalid_input() {
        assert_eq!(base64::encode(b"\x00\xff"), "AP8=");
        assert_eq!(base64::decode("AP8=").unwrap(), b"\x00\xff");
        assert!(base64::decode("!").is_err());
    }

    #[test]
    fn known_hashes_are_real_digests() {
        assert_eq!(hash(b"abc", "sha256").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hash(b"abc", "md5").unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hash(b"abc", "sha3-256").unwrap(), "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532");
        assert!(hash(b"abc", "bogus").is_err());
    }

    #[test]
    fn crypto_known_vectors_binary_and_rejected_inputs() {
        assert_eq!(hmac(b"key", b"The quick brown fox jumps over the lazy dog", "sha256").unwrap(),
            "97yD9DBThCSxMpjmqm+xQ+9NWaFJRhdZl0edvC0aPNg=");
        assert!(hmac(b"key", b"value", "bogus").is_err());
        let lua = Lua::new();
        let env = lua.globals();
        let caps = crate::native::Capabilities::default();
        install(&lua, &env, &caps).unwrap();
        lua.load(r#"
            local key = 'AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8='
            local iv = 'AAECAwQFBgcICQoLDA0ODw=='
            local plain = 'UNC AES fixture\0\255'
            local expected = 'Mi/XrVQIV+rp5vhCqXd8JlMkN3Nxo+EiPiNI1y6053w='
            local encrypted, returnedIv = crypt.encrypt(plain, key, iv, 'CBC')
            assert(encrypted == expected and returnedIv == iv)
            assert(crypt.decrypt(expected, key, iv, 'CBC') == plain)
            local iv32 = crypt.base64encode(crypt.base64decode(iv) .. string.rep('\0', 16))
            assert(crypt.encrypt(plain, key, iv32, 'CBC') == expected)
            assert(crypt.decrypt(expected, key, iv32, 'CBC') == plain)
            local output, randomIv = crypt.encrypt('', key)
            assert(#crypt.base64decode(randomIv) == 16 and crypt.decrypt(output, key, randomIv) == '')
            assert(#crypt.random(16) == 16 and #crypt.random(0) == 0)
            assert(crypt.hmac('key', 'The quick brown fox jumps over the lazy dog', 'sha256') == '97yD9DBThCSxMpjmqm+xQ+9NWaFJRhdZl0edvC0aPNg=')
            assert(not pcall(crypt.encrypt, plain, 'not-base64', iv, 'CBC'))
            assert(not pcall(crypt.encrypt, plain, key, crypt.generatebytes(15), 'CBC'))
            assert(not pcall(crypt.encrypt, plain, key, crypt.generatebytes(17), 'CBC'))
            assert(not pcall(crypt.encrypt, plain, key, iv, 'GCM'))
            assert(not pcall(crypt.decrypt, 'AA==', key, iv, 'CBC'))
            assert(not pcall(crypt.random, 8 * 1024 * 1024 + 1))
        "#).exec().unwrap();
    }

    #[test]
    fn macsploit_explicit_iv_returns_no_second_result() {
        let lua=Lua::new();
        crate::native::set_executor_profile(&lua,"macsploit-744.4").unwrap();
        install(&lua,&lua.globals(),&crate::native::Capabilities::default()).unwrap();
        lua.load(r#"
            local key = crypt.base64encode(string.rep('\1',32))
            local iv = crypt.base64encode(string.rep('\2',16))
            local ciphertext, returned = crypt.encrypt('test', key, iv, 'CBC')
            assert(ciphertext == 'bUFw89utebCs5p20WRotYw==' and returned == nil)
            assert(table.pack(crypt.encrypt('test', key, iv, 'CBC')).n == 1)
            local a, b = string.rep('A', 16), string.rep('B', 16)
            local function encrypt(raw_iv) return table.pack(crypt.encrypt('probe', key, crypt.base64encode(raw_iv), 'CBC')) end
            local aa, bb, ab, ba = encrypt(a), encrypt(b), encrypt(a .. b), encrypt(b .. a)
            assert(ab.n == 1 and ba.n == 1)
            assert(ab[1] == aa[1] and ab[1] ~= bb[1] and ba[1] == bb[1] and ba[1] ~= aa[1])
            assert(crypt.decrypt(ciphertext,key,iv,'CBC') == 'test')
        "#).exec().unwrap();
    }

    #[test]
    fn executor_environment_exposes_aliases_and_binary_roundtrips() {
        let lua = Lua::new();
        let env = lua.globals();
        install(&lua, &env, &crate::native::Capabilities::default()).unwrap();
        lua.load(r#"
            local bytes = "\000\255Hello, world! Hello, world!"
            assert(base64decode(base64encode(bytes)) == bytes)
            assert(crypt.base64.encode(bytes) == base64.encode(bytes))
            assert(crypt.base64_decode(crypt.base64encode(bytes)) == bytes)
            assert(not pcall(base64decode, "!"))
            local packed = lz4compress(bytes)
            assert(lz4decompress(packed) == bytes and lz4decompress(packed, #bytes) == bytes)
            assert(#crypt.base64decode(crypt.generatekey()) == 32)
            assert(#crypt.base64decode(crypt.generatebytes(10)) == 10)
            assert(crypt.hash("abc", "sha1") == "a9993e364706816aba3e25717850c26c9cd0d89d")
        "#).exec().unwrap();
    }
}
