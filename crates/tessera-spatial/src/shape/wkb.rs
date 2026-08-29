//! A reader for OGC well-known binary — `Polygon` and `MultiPolygon` only, either byte order,
//! Z and M ordinates skipped, an EWKB SRID skipped.
//!
//! A polygon in a table arrives as WKB because that is what GeoParquet writes and what every GIS
//! tool exports (`polygon-membership.md` §4.2). Nothing else is read: a `Point`, a `LineString`
//! or a collection is a refusal naming the type, since none of them is a shape with an inside.

use super::canon::RingsF64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WkbError {
    Truncated,
    BadByteOrder(u8),
    /// A geometry type this reader does not accept, by its OGC code with the Z/M/SRID flags
    /// stripped.
    NotAPolygon(u32),
    NotFinite,
    Trailing,
}

impl std::fmt::Display for WkbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WkbError::Truncated => write!(f, "WKB ends before the geometry does"),
            WkbError::BadByteOrder(b) => write!(f, "WKB byte-order flag {b} is neither 0 nor 1"),
            WkbError::NotAPolygon(t) => write!(
                f,
                "WKB geometry type {t} is not Polygon (3) or MultiPolygon (6)"
            ),
            WkbError::NotFinite => write!(f, "a WKB coordinate is not finite"),
            WkbError::Trailing => write!(f, "bytes remain after the WKB geometry"),
        }
    }
}

impl std::error::Error for WkbError {}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    little: bool,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], WkbError> {
        let end = self.at.checked_add(N).ok_or(WkbError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(WkbError::Truncated)?;
        self.at = end;
        Ok(slice.try_into().expect("length checked"))
    }
    fn u32(&mut self) -> Result<u32, WkbError> {
        let b = self.take::<4>()?;
        Ok(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }
    fn f64(&mut self) -> Result<f64, WkbError> {
        let b = self.take::<8>()?;
        let v = if self.little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        };
        if v.is_finite() {
            Ok(v)
        } else {
            Err(WkbError::NotFinite)
        }
    }

    /// The byte-order flag and the type word of one geometry, with the flags decoded: returns
    /// the base type and how many ordinates each point carries.
    fn header(&mut self) -> Result<(u32, usize), WkbError> {
        let order = self.take::<1>()?[0];
        self.little = match order {
            0 => false,
            1 => true,
            b => return Err(WkbError::BadByteOrder(b)),
        };
        let raw = self.u32()?;
        // EWKB flags: Z 0x8000_0000, M 0x4000_0000, SRID 0x2000_0000. ISO WKB: type + 1000 (Z),
        // + 2000 (M), + 3000 (ZM).
        let mut dims = 2;
        if raw & 0x8000_0000 != 0 {
            dims += 1;
        }
        if raw & 0x4000_0000 != 0 {
            dims += 1;
        }
        if raw & 0x2000_0000 != 0 {
            self.u32()?; // the SRID, which the declared `space` supersedes
        }
        let mut base = raw & 0x1FFF_FFFF;
        if base >= 1000 {
            let iso = base / 1000;
            base %= 1000;
            dims = match iso {
                1 | 2 => 3,
                _ => 4,
            };
        }
        Ok((base, dims))
    }

    fn ring(&mut self, dims: usize) -> Result<Vec<(f64, f64)>, WkbError> {
        let n = self.u32()? as usize;
        let mut ring = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let x = self.f64()?;
            let y = self.f64()?;
            for _ in 2..dims {
                self.f64()?;
            }
            ring.push((x, y));
        }
        Ok(ring)
    }

    fn polygon(&mut self, dims: usize) -> Result<Vec<Vec<(f64, f64)>>, WkbError> {
        let rings = self.u32()? as usize;
        let mut out = Vec::with_capacity(rings.min(1 << 16));
        for _ in 0..rings {
            out.push(self.ring(dims)?);
        }
        Ok(out)
    }
}

/// Read a `Polygon` or `MultiPolygon` as parts → rings → `(x, y)`, exactly as encoded: no
/// canonicalisation, closing vertices kept, and the whole buffer consumed.
pub fn read_wkb(bytes: &[u8]) -> Result<RingsF64, WkbError> {
    let mut r = Reader {
        bytes,
        at: 0,
        little: true,
    };
    let (base, dims) = r.header()?;
    let parts = match base {
        3 => vec![r.polygon(dims)?],
        6 => {
            let n = r.u32()? as usize;
            let mut parts = Vec::with_capacity(n.min(1 << 16));
            for _ in 0..n {
                let (inner, inner_dims) = r.header()?;
                if inner != 3 {
                    return Err(WkbError::NotAPolygon(inner));
                }
                parts.push(r.polygon(inner_dims)?);
            }
            parts
        }
        t => return Err(WkbError::NotAPolygon(t)),
    };
    if r.at != bytes.len() {
        return Err(WkbError::Trailing);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le_polygon(rings: &[&[(f64, f64)]], type_word: u32, dims: usize) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend_from_slice(&type_word.to_le_bytes());
        b.extend_from_slice(&(rings.len() as u32).to_le_bytes());
        for ring in rings {
            b.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            for (x, y) in ring.iter() {
                b.extend_from_slice(&x.to_le_bytes());
                b.extend_from_slice(&y.to_le_bytes());
                for _ in 2..dims {
                    b.extend_from_slice(&0f64.to_le_bytes());
                }
            }
        }
        b
    }

    #[test]
    fn a_polygon_reads_in_either_byte_order_with_z_skipped() {
        let ring: &[(f64, f64)] = &[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 0.0)];
        let le = le_polygon(&[ring], 3, 2);
        assert_eq!(read_wkb(&le).unwrap(), vec![vec![ring.to_vec()]]);
        let z = le_polygon(&[ring], 1003, 3);
        assert_eq!(read_wkb(&z).unwrap(), vec![vec![ring.to_vec()]]);
        let ewkb_z = le_polygon(&[ring], 3 | 0x8000_0000, 3);
        assert_eq!(read_wkb(&ewkb_z).unwrap(), vec![vec![ring.to_vec()]]);
        // Big-endian: byte 0, then every word swapped.
        let mut be = vec![0u8];
        be.extend_from_slice(&3u32.to_be_bytes());
        be.extend_from_slice(&1u32.to_be_bytes());
        be.extend_from_slice(&(ring.len() as u32).to_be_bytes());
        for (x, y) in ring {
            be.extend_from_slice(&x.to_be_bytes());
            be.extend_from_slice(&y.to_be_bytes());
        }
        assert_eq!(read_wkb(&be).unwrap(), vec![vec![ring.to_vec()]]);
    }

    #[test]
    fn a_multipolygon_reads_and_a_point_refuses() {
        let ring: &[(f64, f64)] = &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)];
        let inner = le_polygon(&[ring], 3, 2);
        let mut b = vec![1u8];
        b.extend_from_slice(&6u32.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&inner);
        b.extend_from_slice(&inner);
        assert_eq!(read_wkb(&b).unwrap().len(), 2);
        let mut point = vec![1u8];
        point.extend_from_slice(&1u32.to_le_bytes());
        point.extend_from_slice(&1f64.to_le_bytes());
        point.extend_from_slice(&1f64.to_le_bytes());
        assert_eq!(read_wkb(&point), Err(WkbError::NotAPolygon(1)));
        assert_eq!(
            read_wkb(&inner[..inner.len() - 3]),
            Err(WkbError::Truncated)
        );
    }
}
