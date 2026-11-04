//! Just enough Mapbox Vector Tile decoding to draw from; malformed input is `None`, never a panic.

pub struct Tile<'a> {
    pub layers: Vec<Layer<'a>>,
}

pub struct Layer<'a> {
    pub name: &'a str,
    pub extent: u32,
    keys: Vec<&'a str>,
    values: Vec<Value<'a>>,
    features: Vec<&'a [u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Str(&'a str),
    Num(f64),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeomType {
    Point,
    Line,
    Polygon,
    Unknown,
}

pub struct Feature<'l, 'a> {
    layer: &'l Layer<'a>,
    pub kind: GeomType,
    tags: Vec<u32>,
    geometry: Vec<u32>,
}

enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(u64),
    Fixed32(u32),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    /// The next field, `None` at the end; `Some(None)` if malformed.
    fn field(&mut self) -> Option<Option<(u64, Field<'a>)>> {
        if self.pos >= self.buf.len() {
            return None;
        }
        Some((|| {
            let key = self.varint()?;
            let f = match key & 7 {
                0 => Field::Varint(self.varint()?),
                1 => Field::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
                2 => {
                    let n = usize::try_from(self.varint()?).ok()?;
                    Field::Bytes(self.take(n)?)
                }
                5 => Field::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().ok()?)),
                _ => return None,
            };
            Some((key >> 3, f))
        })())
    }
}

fn fields<'a>(buf: &'a [u8], mut f: impl FnMut(u64, Field<'a>) -> Option<()>) -> Option<()> {
    let mut r = Reader::new(buf);
    while let Some(next) = r.field() {
        let (n, v) = next?;
        f(n, v)?;
    }
    Some(())
}

fn packed(buf: &[u8]) -> Option<Vec<u32>> {
    let mut r = Reader::new(buf);
    let mut out = Vec::new();
    while r.pos < buf.len() {
        out.push(u32::try_from(r.varint()?).ok()?);
    }
    Some(out)
}

pub fn decode(buf: &[u8]) -> Option<Tile<'_>> {
    let mut layers = Vec::new();
    fields(buf, |n, f| {
        if let (3, Field::Bytes(b)) = (n, f) {
            layers.push(layer(b)?);
        }
        Some(())
    })?;
    Some(Tile { layers })
}

fn layer(buf: &[u8]) -> Option<Layer<'_>> {
    let mut l = Layer {
        name: "",
        extent: 4096,
        keys: vec![],
        values: vec![],
        features: vec![],
    };
    fields(buf, |n, f| {
        match (n, f) {
            (1, Field::Bytes(b)) => l.name = std::str::from_utf8(b).ok()?,
            (2, Field::Bytes(b)) => l.features.push(b),
            (3, Field::Bytes(b)) => l.keys.push(std::str::from_utf8(b).ok()?),
            (4, Field::Bytes(b)) => l.values.push(value(b)?),
            (5, Field::Varint(v)) => l.extent = u32::try_from(v).ok().filter(|&e| e > 0)?,
            _ => {}
        }
        Some(())
    })?;
    Some(l)
}

fn value(buf: &[u8]) -> Option<Value<'_>> {
    let mut v = Value::Bool(false);
    fields(buf, |n, f| {
        v = match (n, f) {
            (1, Field::Bytes(b)) => Value::Str(std::str::from_utf8(b).ok()?),
            (2, Field::Fixed32(x)) => Value::Num(f64::from(f32::from_bits(x))),
            (3, Field::Fixed64(x)) => Value::Num(f64::from_bits(x)),
            (4 | 5, Field::Varint(x)) => Value::Num(x as i64 as f64),
            (6, Field::Varint(x)) => Value::Num(zigzag(x) as f64),
            (7, Field::Varint(x)) => Value::Bool(x != 0),
            _ => v,
        };
        Some(())
    })?;
    Some(v)
}

fn zigzag(x: u64) -> i64 {
    ((x >> 1) as i64) ^ -((x & 1) as i64)
}

impl<'a> Layer<'a> {
    pub fn features(&self) -> impl Iterator<Item = Option<Feature<'_, 'a>>> + '_ {
        self.features.iter().map(move |b| {
            let mut ft = Feature {
                layer: self,
                kind: GeomType::Unknown,
                tags: vec![],
                geometry: vec![],
            };
            fields(b, |n, f| {
                match (n, f) {
                    (2, Field::Bytes(b)) => ft.tags = packed(b)?,
                    (3, Field::Varint(t)) => {
                        ft.kind = match t {
                            1 => GeomType::Point,
                            2 => GeomType::Line,
                            3 => GeomType::Polygon,
                            _ => GeomType::Unknown,
                        }
                    }
                    (4, Field::Bytes(b)) => ft.geometry = packed(b)?,
                    _ => {}
                }
                Some(())
            })?;
            Some(ft)
        })
    }
}

impl<'a> Feature<'_, 'a> {
    pub fn get(&self, key: &str) -> Option<Value<'a>> {
        self.tags.chunks_exact(2).find_map(|kv| {
            let k = self.layer.keys.get(kv[0] as usize)?;
            (*k == key).then(|| self.layer.values.get(kv[1] as usize).copied())?
        })
    }

    pub fn str(&self, key: &str) -> Option<&'a str> {
        match self.get(key)? {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Each MoveTo starts a part (a line, or a polygon ring), in tile coordinates.
    pub fn parts(&self) -> Option<Vec<Vec<(i32, i32)>>> {
        let (mut x, mut y) = (0i32, 0i32);
        let mut parts: Vec<Vec<(i32, i32)>> = Vec::new();
        let mut g = self.geometry.iter().copied();
        while let Some(cmd) = g.next() {
            let (id, count) = (cmd & 7, cmd >> 3);
            match id {
                1 | 2 => {
                    for i in 0..count {
                        let dx = zigzag(u64::from(g.next()?)) as i32;
                        let dy = zigzag(u64::from(g.next()?)) as i32;
                        x = x.wrapping_add(dx);
                        y = y.wrapping_add(dy);
                        if id == 1 || (i == 0 && parts.is_empty()) {
                            parts.push(Vec::new());
                        }
                        parts.last_mut()?.push((x, y));
                    }
                }
                7 => {}
                _ => return None,
            }
        }
        Some(parts)
    }
}
