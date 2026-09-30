//! A reader for OGC well-known text: `POLYGON` and `MULTIPOLYGON`, with `Z`, `M` and `ZM`
//! ordinates skipped and `EMPTY` accepted as no rings.
//!
//! The inline spelling for a shape in `corpus.toml`: what every tool prints, and what a person
//! can type.

use super::canon::RingsF64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WktError {
    /// The leading keyword is not one this reader accepts.
    NotAPolygon(String),
    /// What was expected, and the byte offset where something else was found.
    Expected(&'static str, usize),
    NotFinite(usize),
}

impl std::fmt::Display for WktError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WktError::NotAPolygon(k) => write!(f, "WKT `{k}` is not POLYGON or MULTIPOLYGON"),
            WktError::Expected(what, at) => write!(f, "WKT: expected {what} at byte {at}"),
            WktError::NotFinite(at) => write!(f, "WKT: a coordinate at byte {at} is not finite"),
        }
    }
}

impl std::error::Error for WktError {}

struct Parser<'a> {
    s: &'a str,
    at: usize,
    dims: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while let Some(c) = self.s[self.at..].chars().next() {
            if c.is_whitespace() {
                self.at += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn word(&mut self) -> &str {
        self.skip_ws();
        let start = self.at;
        while let Some(c) = self.s[self.at..].chars().next() {
            if c.is_ascii_alphabetic() {
                self.at += 1;
            } else {
                break;
            }
        }
        &self.s[start..self.at]
    }

    fn punct(&mut self, ch: char) -> bool {
        self.skip_ws();
        if self.s[self.at..].starts_with(ch) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, ch: char, what: &'static str) -> Result<(), WktError> {
        if self.punct(ch) {
            Ok(())
        } else {
            Err(WktError::Expected(what, self.at))
        }
    }

    fn number(&mut self) -> Result<f64, WktError> {
        self.skip_ws();
        let start = self.at;
        while let Some(c) = self.s[self.at..].chars().next() {
            if c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E') {
                self.at += 1;
            } else {
                break;
            }
        }
        let v: f64 = self.s[start..self.at]
            .parse()
            .map_err(|_| WktError::Expected("a number", start))?;
        if v.is_finite() {
            Ok(v)
        } else {
            Err(WktError::NotFinite(start))
        }
    }

    fn point(&mut self) -> Result<(f64, f64), WktError> {
        let x = self.number()?;
        let y = self.number()?;
        for _ in 2..self.dims {
            self.number()?;
        }
        Ok((x, y))
    }

    fn ring(&mut self) -> Result<Vec<(f64, f64)>, WktError> {
        self.expect('(', "`(` opening a ring")?;
        let mut ring = vec![self.point()?];
        while self.punct(',') {
            ring.push(self.point()?);
        }
        self.expect(')', "`)` closing a ring")?;
        Ok(ring)
    }

    fn polygon(&mut self) -> Result<Vec<Vec<(f64, f64)>>, WktError> {
        if self.word().eq_ignore_ascii_case("EMPTY") {
            return Ok(Vec::new());
        }
        self.expect('(', "`(` opening a polygon")?;
        let mut rings = vec![self.ring()?];
        while self.punct(',') {
            rings.push(self.ring()?);
        }
        self.expect(')', "`)` closing a polygon")?;
        Ok(rings)
    }

    /// The optional `Z`, `M` or `ZM` after the keyword.
    fn dims(&mut self) {
        let save = self.at;
        let w = self.word().to_ascii_uppercase();
        self.dims = match w.as_str() {
            "Z" | "M" => 3,
            "ZM" => 4,
            _ => {
                self.at = save;
                2
            }
        };
    }
}

/// Read `POLYGON (…)` or `MULTIPOLYGON (…)` as parts → rings → `(x, y)`, exactly as written.
pub fn read_wkt(s: &str) -> Result<RingsF64, WktError> {
    let mut p = Parser { s, at: 0, dims: 2 };
    let keyword = p.word().to_ascii_uppercase();
    let parts = match keyword.as_str() {
        "POLYGON" => {
            p.dims();
            let rings = p.polygon()?;
            if rings.is_empty() {
                Vec::new()
            } else {
                vec![rings]
            }
        }
        "MULTIPOLYGON" => {
            p.dims();
            if p.word().eq_ignore_ascii_case("EMPTY") {
                Vec::new()
            } else {
                p.expect('(', "`(` opening a multipolygon")?;
                let mut parts = vec![p.polygon()?];
                while p.punct(',') {
                    parts.push(p.polygon()?);
                }
                p.expect(')', "`)` closing a multipolygon")?;
                parts
            }
        }
        other => return Err(WktError::NotAPolygon(other.to_string())),
    };
    p.skip_ws();
    if p.at != s.len() {
        return Err(WktError::Expected("the end of the text", p.at));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polygons_with_holes_and_multipolygons_read() {
        let p = read_wkt("POLYGON ((0 0, 10 0, 10 10, 0 10, 0 0), (2 2, 3 2, 3 3, 2 2))").unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].len(), 2);
        assert_eq!(p[0][1][1], (3.0, 2.0));
        let m = read_wkt("multipolygon(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5)))").unwrap();
        assert_eq!(m.len(), 2);
        let z = read_wkt("POLYGON Z ((0 0 7, 1 0 7, 1 1 7, 0 0 7))").unwrap();
        assert_eq!(z[0][0][2], (1.0, 1.0));
        assert!(read_wkt("POLYGON EMPTY").unwrap().is_empty());
        assert!(read_wkt("MULTIPOLYGON EMPTY").unwrap().is_empty());
    }

    #[test]
    fn what_is_not_a_polygon_refuses_with_a_place() {
        assert_eq!(
            read_wkt("POINT (1 2)"),
            Err(WktError::NotAPolygon("POINT".to_string()))
        );
        assert!(matches!(
            read_wkt("POLYGON ((0 0, 1 0, 1 1"),
            Err(WktError::Expected(_, _))
        ));
        assert!(matches!(
            read_wkt("POLYGON ((0 0, 1 0, 1 1, 0 0)) extra"),
            Err(WktError::Expected("the end of the text", _))
        ));
    }
}
