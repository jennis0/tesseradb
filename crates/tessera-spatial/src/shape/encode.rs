//! The canonical bytes of a shape (`polygon-membership.md` §6.6) — what the record blob holds.
//!
//! ```text
//! shape := u8 1 | 4 × u32 LE                          -- bbox: min_x, min_y, max_x, max_y
//!        | u8 2 | 2 × u32 LE | 3 × f64 LE             -- conic: cx, cy, m11, m12, m22
//!        | u8 4 | u16 LE parts                        -- polygon
//!                | per part:  u16 LE rings
//!                | per ring:  u32 LE n | n × (u32 LE x, u32 LE y, u32 LE weight)
//! ```
//!
//! Tag 0 (*no shape*) and tag 3 are the blob's, not this module's: the blob writes 0 for an
//! artifact without a shape and decodes it before asking here. Every length is explicit, so a
//! truncated buffer is a refusal and never a shorter shape.

use super::conic::Conic;
use super::polygon::{Part, Polygon, Ring, Vertex};
use super::{Bbox, Shape};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    UnknownTag(u8),
    /// A ring of fewer than three vertices, which the canonical form never produces.
    DegenerateRing,
    NotFinite,
    Trailing,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "shape bytes end before the shape does"),
            DecodeError::UnknownTag(t) => write!(f, "shape tag {t} is not one this reader decodes"),
            DecodeError::DegenerateRing => write!(f, "a ring of fewer than three vertices"),
            DecodeError::NotFinite => write!(f, "a conic coefficient is not finite"),
            DecodeError::Trailing => write!(f, "bytes remain after the shape"),
        }
    }
}

impl std::error::Error for DecodeError {}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let end = self.at.checked_add(N).ok_or(DecodeError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(DecodeError::Truncated)?;
        self.at = end;
        Ok(slice.try_into().expect("length checked"))
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn f64(&mut self) -> Result<f64, DecodeError> {
        let v = f64::from_le_bytes(self.take()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(DecodeError::NotFinite)
        }
    }
}

impl Shape {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Shape::Bbox(b) => {
                out.push(1);
                for v in [b.min_x, b.min_y, b.max_x, b.max_y] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Shape::Conic(c) => {
                out.push(2);
                out.extend_from_slice(&c.cx.to_le_bytes());
                out.extend_from_slice(&c.cy.to_le_bytes());
                for v in [c.m11, c.m12, c.m22] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Shape::Polygon(p) => {
                out.push(4);
                out.extend_from_slice(&(p.parts.len() as u16).to_le_bytes());
                for part in &p.parts {
                    out.extend_from_slice(&(part.rings.len() as u16).to_le_bytes());
                    for ring in &part.rings {
                        out.extend_from_slice(&(ring.vertices.len() as u32).to_le_bytes());
                        for v in &ring.vertices {
                            out.extend_from_slice(&v.x.to_le_bytes());
                            out.extend_from_slice(&v.y.to_le_bytes());
                            out.extend_from_slice(&v.weight.to_le_bytes());
                        }
                    }
                }
            }
        }
    }

    /// Decode one shape occupying the whole of `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Shape, DecodeError> {
        let (shape, used) = Shape::decode_prefix(bytes)?;
        if used == bytes.len() {
            Ok(shape)
        } else {
            Err(DecodeError::Trailing)
        }
    }

    /// Decode a shape from the front of `bytes`, returning how many bytes it occupied.
    pub fn decode_prefix(bytes: &[u8]) -> Result<(Shape, usize), DecodeError> {
        let mut r = Reader { bytes, at: 0 };
        let shape = match r.u8()? {
            1 => Shape::Bbox(Bbox {
                min_x: r.u32()?,
                min_y: r.u32()?,
                max_x: r.u32()?,
                max_y: r.u32()?,
            }),
            2 => Shape::Conic(Conic {
                cx: r.u32()?,
                cy: r.u32()?,
                m11: r.f64()?,
                m12: r.f64()?,
                m22: r.f64()?,
            }),
            4 => {
                let parts = r.u16()?;
                let mut polygon = Polygon::default();
                for _ in 0..parts {
                    let rings = r.u16()?;
                    let mut part = Part { rings: Vec::new() };
                    for _ in 0..rings {
                        let n = r.u32()?;
                        if n < 3 {
                            return Err(DecodeError::DegenerateRing);
                        }
                        let mut vertices = Vec::with_capacity(n.min(1 << 20) as usize);
                        for _ in 0..n {
                            vertices.push(Vertex {
                                x: r.u32()?,
                                y: r.u32()?,
                                weight: r.u32()?,
                            });
                        }
                        part.rings.push(Ring { vertices });
                    }
                    polygon.parts.push(part);
                }
                Shape::Polygon(polygon)
            }
            t => return Err(DecodeError::UnknownTag(t)),
        };
        Ok((shape, r.at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_and_a_short_buffer_refuses() {
        let v = |x, y, weight| Vertex { x, y, weight };
        let shapes = [
            Shape::Bbox(Bbox {
                min_x: 1,
                min_y: 2,
                max_x: 3,
                max_y: 4,
            }),
            Shape::Conic(Conic {
                cx: 5,
                cy: 6,
                m11: 0.5,
                m12: -0.25,
                m22: 2.0,
            }),
            Shape::Polygon(Polygon {
                parts: vec![Part {
                    rings: vec![
                        Ring {
                            vertices: vec![v(0, 0, u32::MAX), v(9, 0, u32::MAX), v(9, 9, u32::MAX)],
                        },
                        Ring {
                            vertices: vec![v(2, 2, 3), v(3, 2, 3), v(3, 3, u32::MAX)],
                        },
                    ],
                }],
            }),
        ];
        for s in shapes {
            let bytes = s.encode();
            assert_eq!(Shape::decode(&bytes).unwrap(), s);
            assert_eq!(
                Shape::decode(&bytes[..bytes.len() - 1]),
                Err(DecodeError::Truncated)
            );
            let mut longer = bytes.clone();
            longer.push(0);
            assert_eq!(Shape::decode(&longer), Err(DecodeError::Trailing));
        }
        assert_eq!(Shape::decode(&[7]), Err(DecodeError::UnknownTag(7)));
    }
}
