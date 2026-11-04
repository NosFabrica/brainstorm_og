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

    if is_hex64(s) {
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

/// A 64-char hex pubkey or event id.
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
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

/// Encode a hex event id as `note1…`.
pub fn note_from_hex(hex_id: &str) -> Option<String> {
    let bytes = hex::decode(hex_id).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    bech32::encode::<Bech32>(Hrp::parse("note").ok()?, &bytes).ok()
}

/// TLV: type(1) | len(1) | value(len). type 0 = 32-byte pubkey, type 1 = relay url.
fn parse_nprofile(data: &[u8]) -> Result<Pointer> {
    let hex_pk = tlv_entries(data)?
        .into_iter()
        .find(|(t, v)| *t == 0 && v.len() == 32)
        .map(|(_, v)| hex::encode(v));
    let hex = hex_pk.ok_or_else(|| anyhow!("nprofile missing a pubkey (TLV type 0)"))?;
    Ok(Pointer { hex })
}

fn tlv_entries(data: &[u8]) -> Result<Vec<(u8, &[u8])>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 <= data.len() {
        let t = data[i];
        let start = i + 2;
        let end = start + data[i + 1] as usize;
        if end > data.len() {
            bail!("TLV overruns buffer");
        }
        out.push((t, &data[start..end]));
        i = end;
    }
    Ok(out)
}

/// What `/e/{id}` names. Relay hints, and nevent's author/kind, are parsed and discarded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Entity {
    Event {
        id: String,
    },
    Address {
        kind: u32,
        pubkey: String,
        d: String,
    },
}

impl Entity {
    /// Stable cache key; prefixed so it never collides with a profile's hex pubkey.
    pub fn key(&self) -> String {
        match self {
            Entity::Event { id } => format!("e:{id}"),
            Entity::Address { kind, pubkey, d } => format!("a:{kind}:{pubkey}:{d}"),
        }
    }

    /// The hint-free bech32 the canonical `/e/` link carries.
    pub fn canonical(&self) -> String {
        let (hrp, data) = match self {
            Entity::Event { id } => {
                let mut data = vec![0u8, 32];
                data.extend(hex::decode(id).unwrap_or_default());
                ("nevent", data)
            }
            Entity::Address { kind, pubkey, d } => {
                let d = &d.as_bytes()[..d.len().min(255)];
                let mut data = vec![0u8, d.len() as u8];
                data.extend_from_slice(d);
                data.extend([2u8, 32]);
                data.extend(hex::decode(pubkey).unwrap_or_default());
                data.extend([3u8, 4]);
                data.extend(kind.to_be_bytes());
                ("naddr", data)
            }
        };
        let hrp = Hrp::parse(hrp).expect("static hrp");
        bech32::encode::<Bech32>(hrp, &data).unwrap_or_default()
    }
}

/// Accept `note1…`, `nevent1…` or `naddr1…`.
pub fn decode_entity(input: &str) -> Result<Entity> {
    let (hrp, data) =
        bech32::decode(input.trim()).map_err(|e| anyhow!("bech32 decode failed: {e}"))?;
    match hrp.to_string().as_str() {
        "note" => {
            if data.len() != 32 {
                bail!("note payload is {} bytes, expected 32", data.len());
            }
            Ok(Entity::Event {
                id: hex::encode(data),
            })
        }
        "nevent" => tlv_entries(&data)?
            .into_iter()
            .find(|(t, v)| *t == 0 && v.len() == 32)
            .map(|(_, v)| Entity::Event { id: hex::encode(v) })
            .ok_or_else(|| anyhow!("nevent missing an id")),
        "naddr" => {
            let (mut d, mut pubkey, mut kind) = (None, None, None);
            for (t, v) in tlv_entries(&data)? {
                match t {
                    0 if d.is_none() => d = Some(String::from_utf8(v.to_vec())?),
                    2 if v.len() == 32 && pubkey.is_none() => pubkey = Some(hex::encode(v)),
                    3 if v.len() == 4 && kind.is_none() => {
                        kind = Some(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
                    }
                    _ => {}
                }
            }
            match (kind, pubkey, d) {
                (Some(kind), Some(pubkey), Some(d)) => Ok(Entity::Address { kind, pubkey, d }),
                _ => bail!("naddr missing kind, pubkey or d"),
            }
        }
        other => bail!("unsupported NIP-19 entity: {other}"),
    }
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

    fn tlv(entries: &[(u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (t, v) in entries {
            out.push(*t);
            out.push(v.len() as u8);
            out.extend_from_slice(v);
        }
        out
    }

    fn bech(hrp: &str, data: &[u8]) -> String {
        bech32::encode::<Bech32>(Hrp::parse(hrp).unwrap(), data).unwrap()
    }

    const ID: &str = "b9f5441e45ca39179320e0031cfb18e34078673dcc3d3e3a3b3a981760aa5696";

    #[test]
    fn decodes_note_and_nevent_dropping_hints() {
        let id = hex::decode(ID).unwrap();
        let note = bech("note", &id);
        assert_eq!(
            decode_entity(&note).unwrap(),
            Entity::Event { id: ID.into() }
        );

        let pk = hex::decode(HEX).unwrap();
        let nevent = bech(
            "nevent",
            &tlv(&[
                (0, &id),
                (1, b"ws://10.0.0.5:6379"),
                (2, &pk),
                (3, &1u32.to_be_bytes()),
            ]),
        );
        assert_eq!(
            decode_entity(&nevent).unwrap(),
            Entity::Event { id: ID.into() }
        );
    }

    #[test]
    fn decodes_naddr_dropping_hints() {
        let pk = hex::decode(HEX).unwrap();
        let naddr = bech(
            "naddr",
            &tlv(&[
                (0, b"my-article"),
                (1, b"wss://a.example"),
                (2, &pk),
                (3, &30023u32.to_be_bytes()),
            ]),
        );
        assert_eq!(
            decode_entity(&naddr).unwrap(),
            Entity::Address {
                kind: 30023,
                pubkey: HEX.into(),
                d: "my-article".into()
            }
        );
    }

    #[test]
    fn rejects_malformed_entities() {
        let id = hex::decode(ID).unwrap();
        assert!(decode_entity(&bech("note", &id[..31])).is_err());
        assert!(decode_entity(&bech("nevent", &tlv(&[(1, b"wss://x")]))).is_err());
        assert!(decode_entity(&bech("nevent", &[0u8, 32, 1, 2])).is_err());
        // naddr without kind or pubkey.
        assert!(
            decode_entity(&bech("naddr", &tlv(&[(0, b"d"), (3, &1u32.to_be_bytes())]))).is_err()
        );
        let pk = hex::decode(HEX).unwrap();
        assert!(decode_entity(&bech("naddr", &tlv(&[(0, b"d"), (2, &pk)]))).is_err());
        // A profile is not an event.
        assert!(decode_entity(&npub_from_hex(HEX).unwrap()).is_err());
        assert!(decode_entity(&bech("nsec", &[7u8; 32])).is_err());
        assert!(decode_entity(ID).is_err());
    }

    #[test]
    fn canonical_drops_everything_but_the_identity() {
        let id = hex::decode(ID).unwrap();
        let pk = hex::decode(HEX).unwrap();
        let hinted = bech(
            "nevent",
            &tlv(&[(0, &id), (1, b"wss://a.example"), (2, &pk)]),
        );
        let plain = Entity::Event { id: ID.into() }.canonical();
        assert!(plain.starts_with("nevent1"));
        assert_ne!(plain, hinted);
        assert_eq!(
            plain,
            decode_entity(&bech("note", &id)).unwrap().canonical()
        );
        assert_eq!(
            decode_entity(&plain).unwrap(),
            Entity::Event { id: ID.into() }
        );

        let addr = Entity::Address {
            kind: 30023,
            pubkey: HEX.into(),
            d: "x".into(),
        };
        let c = addr.canonical();
        assert!(c.starts_with("naddr1"));
        assert_eq!(decode_entity(&c).unwrap(), addr);
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
