use anyhow::{anyhow, bail, Result};

/// A decoded NIP-19 reference to a pubkey, plus any relay hints (nprofile).
#[derive(Debug, Clone)]
pub struct Pointer {
    pub hex: String,
    pub relays: Vec<String>,
}

/// Accept raw hex, `npub1…`, or `nprofile1…` and return the 64-char hex pubkey
/// (+ relay hints when the entity carries them).
pub fn decode(input: &str) -> Result<Pointer> {
    let s = input.trim();

    if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(Pointer { hex: s.to_lowercase(), relays: vec![] });
    }

    let (hrp, data) = bech32::decode(s).map_err(|e| anyhow!("bech32 decode failed: {e}"))?;
    match hrp.to_string().as_str() {
        "npub" => {
            if data.len() != 32 {
                bail!("npub payload is {} bytes, expected 32", data.len());
            }
            Ok(Pointer { hex: hex::encode(data), relays: vec![] })
        }
        "nprofile" => parse_nprofile(&data),
        other => bail!("unsupported NIP-19 entity: {other}"),
    }
}

/// TLV: type(1) | len(1) | value(len). type 0 = 32-byte pubkey, type 1 = relay url.
fn parse_nprofile(data: &[u8]) -> Result<Pointer> {
    let mut hex_pk: Option<String> = None;
    let mut relays = Vec::new();
    let mut i = 0usize;

    while i + 2 <= data.len() {
        let t = data[i];
        let len = data[i + 1] as usize;
        let start = i + 2;
        let end = start + len;
        if end > data.len() {
            bail!("nprofile TLV overruns buffer");
        }
        let val = &data[start..end];
        match t {
            0 if val.len() == 32 => hex_pk = Some(hex::encode(val)),
            1 => {
                if let Ok(r) = std::str::from_utf8(val) {
                    if !r.is_empty() {
                        relays.push(r.to_string());
                    }
                }
            }
            _ => {}
        }
        i = end;
    }

    let hex = hex_pk.ok_or_else(|| anyhow!("nprofile missing a pubkey (TLV type 0)"))?;
    Ok(Pointer { hex, relays })
}
