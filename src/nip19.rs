use anyhow::{anyhow, bail, Result};
use bech32::{Bech32, Hrp};

/// A decoded NIP-19 reference to a pubkey.
///
/// Accepts exactly what `Brainstorm-UI`'s `decodeShareId` accepts — hex,
/// `npub1…`, `nprofile1…` — so any `/p/:id` the app can render, this can unfurl.
///
/// `nprofile` relay hints are parsed, because the TLV walk must step over them,
/// then discarded: we only ever query the relay in config, so a hostile
/// `nprofile` carrying `ws://10.0.0.5:6379` has nothing to reach.
#[derive(Debug, Clone)]
pub struct Pointer {
    pub hex: String,
}

/// Accept raw hex, `npub1…`, or `nprofile1…` and return the 64-char hex pubkey.
pub fn decode(input: &str) -> Result<Pointer> {
    let s = input.trim();

    if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(Pointer {
            hex: s.to_lowercase(),
        });
    }

    let (hrp, data) = bech32::decode(s).map_err(|e| anyhow!("bech32 decode failed: {e}"))?;
    match hrp.to_string().as_str() {
        "npub" => {
            if data.len() != 32 {
                bail!("npub payload is {} bytes, expected 32", data.len());
            }
            Ok(Pointer {
                hex: hex::encode(data),
            })
        }
        "nprofile" => parse_nprofile(&data),
        // Notably this rejects `nsec` — a secret key must never be treated as
        // an identifier we look up and render.
        other => bail!("unsupported NIP-19 entity: {other}"),
    }
}

/// Encode a hex pubkey back to `npub1…`, for the truncated-name fallback.
pub fn npub_from_hex(hex_pk: &str) -> Option<String> {
    let bytes = hex::decode(hex_pk).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let hrp = Hrp::parse("npub").ok()?;
    bech32::encode::<Bech32>(hrp, &bytes).ok()
}

/// TLV: type(1) | len(1) | value(len). type 0 = 32-byte pubkey, type 1 = relay url.
fn parse_nprofile(data: &[u8]) -> Result<Pointer> {
    let mut hex_pk: Option<String> = None;
    let mut i = 0usize;

    while i + 2 <= data.len() {
        let t = data[i];
        let len = data[i + 1] as usize;
        let start = i + 2;
        let end = start + len;
        if end > data.len() {
            bail!("nprofile TLV overruns buffer");
        }
        if t == 0 && end - start == 32 {
            hex_pk = Some(hex::encode(&data[start..end]));
        }
        i = end;
    }

    let hex = hex_pk.ok_or_else(|| anyhow!("nprofile missing a pubkey (TLV type 0)"))?;
    Ok(Pointer { hex })
}

#[cfg(test)]
mod tests {
    use super::*;

    // vitalik-ish well-known test vector: a valid 32-byte key.
    const HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

    #[test]
    fn accepts_hex_any_case() {
        assert_eq!(decode(HEX).unwrap().hex, HEX);
        assert_eq!(decode(&HEX.to_uppercase()).unwrap().hex, HEX);
        assert_eq!(decode(&format!("  {HEX}  ")).unwrap().hex, HEX);
    }

    #[test]
    fn roundtrips_npub() {
        let npub = npub_from_hex(HEX).unwrap();
        assert!(npub.starts_with("npub1"));
        assert_eq!(decode(&npub).unwrap().hex, HEX);
    }

    #[test]
    fn accepts_nprofile_and_ignores_hints() {
        // TLV: type 0 (pubkey, 32B) then two type-1 relay hints.
        let pk = hex::decode(HEX).unwrap();
        let mut tlv = vec![0u8, 32];
        tlv.extend_from_slice(&pk);
        for relay in ["wss://a.example", "ws://10.0.0.5:6379"] {
            tlv.push(1);
            tlv.push(relay.len() as u8);
            tlv.extend_from_slice(relay.as_bytes());
        }
        let hrp = Hrp::parse("nprofile").unwrap();
        let encoded = bech32::encode::<Bech32>(hrp, &tlv).unwrap();

        // The pubkey survives; the internal-address hint has nowhere to go.
        assert_eq!(decode(&encoded).unwrap().hex, HEX);
    }

    #[test]
    fn rejects_malformed_input() {
        // TLV length running past the end must not panic or over-read.
        let hrp = Hrp::parse("nprofile").unwrap();
        let overrun = bech32::encode::<Bech32>(hrp, &[0u8, 32, 0, 0]).unwrap();
        assert!(decode(&overrun).is_err());

        // nprofile with no type-0 entry.
        let hrp = Hrp::parse("nprofile").unwrap();
        let no_pk = bech32::encode::<Bech32>(hrp, &[1u8, 1, b'x']).unwrap();
        assert!(decode(&no_pk).is_err());

        assert!(decode(&HEX[..63]).is_err());
        assert!(decode("not-bech32-at-all").is_err());
        assert!(decode("").is_err());
        // Wrong payload length behind a valid npub hrp.
        let hrp = Hrp::parse("npub").unwrap();
        assert!(decode(&bech32::encode::<Bech32>(hrp, &[0u8; 31]).unwrap()).is_err());
    }

    #[test]
    fn rejects_nsec() {
        let hrp = Hrp::parse("nsec").unwrap();
        let nsec = bech32::encode::<Bech32>(hrp, &[7u8; 32]).unwrap();
        assert!(
            decode(&nsec).is_err(),
            "a secret key must never be accepted as an id"
        );
    }
}
