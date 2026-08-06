//! Stable per-device identity for the free tier.
//!
//! Derived from a hardware id rather than a random UUID, so that reinstalling --
//! or simply deleting the config file -- does not present as a brand-new device
//! with a brand-new daily budget. That raises the cost of resetting from
//! "delete a file" to "spoof a machine id"; it does not make it impossible, and
//! the gateway's fleet-wide cap is what actually bounds the damage.
//!
//! The **raw** hardware id never leaves the machine: what is sent is its
//! SHA-256. The server salts that again with a secret pepper before storing it,
//! so neither side holds anything that identifies the hardware.

use std::process::Command;

/// SHA-256 of the machine's hardware id, lowercase hex.
///
/// Falls back to a random value persisted in the config when no hardware id can
/// be read (containers, hardened systems, unusual platforms). Such a device gets
/// a fresh budget if it is ever regenerated, which is the correct trade: failing
/// closed here would deny the free tier to legitimate users on ordinary setups.
pub fn device_id_hash(fallback: &str) -> String {
    let raw = hardware_id().unwrap_or_else(|| fallback.to_string());
    sha256_hex(raw.trim().as_bytes())
}

fn hardware_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // IOPlatformUUID is stable across reinstalls and OS upgrades.
        let out = Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.contains("IOPlatformUUID") {
                if let Some(value) = line.split('=').nth(1) {
                    let cleaned = value.trim().trim_matches('"').trim();
                    if !cleaned.is_empty() {
                        return Some(cleaned.to_string());
                    }
                }
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(text) = std::fs::read_to_string(path) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }

    #[cfg(target_os = "windows")]
    {
        let out = Command::new("reg")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find(|l| l.contains("MachineGuid"))
            .and_then(|l| l.split_whitespace().last())
            .map(|s| s.to_string())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// A random identifier, persisted in config, for machines with no readable
/// hardware id.
pub fn random_fallback_id() -> String {
    // Not cryptographic: this only needs to be unlikely to collide. Mixing the
    // clock with the address of a heap allocation avoids pulling in a crate for
    // it.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let boxed = Box::new(0u8);
    let addr = &*boxed as *const u8 as usize;
    sha256_hex(format!("{nanos}:{addr}").as_bytes())
}

// ---- SHA-256 ---------------------------------------------------------------
// Implemented here rather than pulled in as a dependency: this is the only
// hashing the client does, and the alternative is another crate in the supply
// chain of a binary that is handed an API key.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub fn sha256_hex(input: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published NIST vectors. A hand-rolled hash is only trustworthy if it is
    /// pinned against known answers.
    #[test]
    fn sha256_matches_the_published_test_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// Exercises the multi-block path and the length-field padding boundary.
    #[test]
    fn sha256_handles_block_boundaries() {
        assert_eq!(
            sha256_hex(&[b'a'; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn the_device_hash_is_stable_across_calls() {
        assert_eq!(device_id_hash("fallback-a"), device_id_hash("fallback-a"));
    }

    #[test]
    fn the_device_hash_is_hex_sha256() {
        let hash = device_id_hash("anything");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    /// The raw hardware id must never appear in what is sent to the server.
    #[test]
    fn the_hash_never_contains_the_raw_input() {
        let raw = "0123456789ABCDEF-A-VERY-DISTINCTIVE-MACHINE-ID";
        assert!(!sha256_hex(raw.as_bytes()).contains("DISTINCTIVE"));
        assert!(!sha256_hex(raw.as_bytes()).contains(raw));
    }

    #[test]
    fn fallback_ids_differ_between_generations() {
        assert_ne!(random_fallback_id(), random_fallback_id());
    }

    /// On a machine with a readable hardware id, the fallback is irrelevant --
    /// which is the whole point of preferring hardware.
    #[test]
    fn a_machine_with_hardware_id_ignores_the_fallback() {
        if hardware_id().is_some() {
            assert_eq!(device_id_hash("one"), device_id_hash("two"));
        }
    }
}
