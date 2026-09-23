//! The cursor a page end carries: a position sealed to the read it belongs to.
//!
//! XChaCha20-Poly1305 under a key derived from the deployment's identity key, with a random
//! 24-byte nonce drawn for each cursor. The route, the format, the view, the view's incarnation, the
//! session's authorisation-data hash and, on the artifacts route, the layer, its entity and the
//! level named are the associated data, so a cursor presented under any other binding does not
//! open, and every such failure is the one refusal
//! [`EngineError::CursorRefused`]. The idset and the order are sealed inside: another idset has a
//! refusal of its own, and a request that names no order takes the cursor's. A client can read
//! nothing from a cursor and can build none, so no position in one is used before it has opened.

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::RecordsOrder;
use crate::error::{EngineError, Result};

const KEY_DOMAIN: &[u8] = b"tessera-records-cursor-key-v1";
const AAD_DOMAIN: &[u8] = b"tessera-records-cursor-v1";
/// The sealed payload's layout. A cursor of another format does not open.
const FORMAT: u8 = 4;
const NONCE_LEN: usize = 24;

/// The route a cursor was issued on, bound into its associated data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Route {
    Items = 1,
    Artifacts = 2,
}

/// What a cursor is bound to besides its payload.
pub(super) struct Binding<'a> {
    pub(super) route: Route,
    pub(super) view: &'a str,
    pub(super) incarnation: u64,
    pub(super) auth_data_hash: [u8; 32],
    /// The layer an artifacts read is of; `None` on the items route.
    pub(super) layer: Option<LayerBinding<'a>>,
}

/// The layer an artifacts cursor is bound to: its name, its own entity, which a drop and a later
/// registration never share, and the level the request named, if any.
pub(super) struct LayerBinding<'a> {
    pub(super) name: &'a str,
    pub(super) entity: u64,
    pub(super) level: Option<u32>,
}

impl Binding<'_> {
    fn associated_data(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(AAD_DOMAIN.len() + 2 + 8 + self.view.len() + 8 + 32 + 32);
        aad.extend_from_slice(AAD_DOMAIN);
        aad.push(FORMAT);
        aad.push(self.route as u8);
        aad.extend_from_slice(&(self.view.len() as u64).to_le_bytes());
        aad.extend_from_slice(self.view.as_bytes());
        aad.extend_from_slice(&self.incarnation.to_le_bytes());
        aad.extend_from_slice(&self.auth_data_hash);
        match &self.layer {
            None => aad.push(0),
            Some(layer) => {
                aad.push(1);
                aad.extend_from_slice(&(layer.name.len() as u64).to_le_bytes());
                aad.extend_from_slice(layer.name.as_bytes());
                aad.extend_from_slice(&layer.entity.to_le_bytes());
                match layer.level {
                    None => aad.push(0),
                    Some(level) => {
                        aad.push(1);
                        aad.extend_from_slice(&level.to_le_bytes());
                    }
                }
            }
        }
        aad
    }
}

/// The sealing key, derived from the identity key when the engine opens, so a rotation of that
/// key stops every cursor opening.
#[derive(Clone, Copy)]
pub(crate) struct CursorKey([u8; 32]);

impl CursorKey {
    /// The key for a deployment whose identity key the manifest spells `identity_key_hex`: 32
    /// lowercase hex characters, which `IdentityKey::from_hex` has already held to that spelling,
    /// so the spelling is one-to-one with the key's bytes and is hashed as it stands.
    pub(crate) fn of(identity_key_hex: &str) -> CursorKey {
        let mut hasher = Sha256::new();
        hasher.update(KEY_DOMAIN);
        hasher.update(identity_key_hex.as_bytes());
        CursorKey(hasher.finalize().into())
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new_from_slice(&self.0).expect("the key is 32 bytes")
    }

    /// `payload` sealed under `binding`, as base64url without padding.
    pub(super) fn seal(&self, binding: &Binding<'_>, payload: &[u8]) -> String {
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let aad = binding.associated_data();
        let sealed = self
            .cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: payload,
                    aad: &aad,
                },
            )
            .expect("sealing a payload of this size cannot fail");
        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
    }

    /// The payload `token` seals under `binding`, or [`EngineError::CursorRefused`] for every
    /// reason it does not open.
    pub(super) fn open(&self, binding: &Binding<'_>, token: &str) -> Result<Vec<u8>> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| EngineError::CursorRefused)?;
        if raw.len() <= NONCE_LEN {
            return Err(EngineError::CursorRefused);
        }
        let (nonce, sealed) = raw.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce length");
        let aad = binding.associated_data();
        self.cipher()
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .map_err(|_| EngineError::CursorRefused)
    }
}

/// A row's place in its order: `(cell, tessera_id)` in map order, and `(item number, 0)` in
/// stored order, where a scan position past a whole stretch is `(n, u64::MAX)` for `n` the number
/// just before the next stretch's first item, which need not be an item. Item numbers are held
/// only inside the seal.
pub(super) type Key = (u32, u64);

/// How far a read has gone: `scan` is the position every row at or before which has been
/// returned or passed over, and a read resumes after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Position {
    pub(super) order: RecordsOrder,
    pub(super) scan: Option<Key>,
}

impl Position {
    /// The start of a read in `order`.
    pub(super) fn start(order: RecordsOrder) -> Position {
        Position { order, scan: None }
    }
}

/// An items cursor's sealed payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ItemsCursor {
    pub(super) idset: u32,
    pub(super) position: Position,
    /// The rows the read's next stretch spans, so a resumed read continues at the size it had
    /// grown to.
    pub(super) stretch: u32,
}

/// `idset, order, has_scan, scan (u32, u64), stretch`, little-endian.
const PAYLOAD_LEN: usize = 4 + 1 + 1 + 12 + 4;

impl ItemsCursor {
    pub(super) fn encode(&self) -> Vec<u8> {
        let Position { order, scan } = self.position;
        let order = match order {
            RecordsOrder::Map => 0u8,
            RecordsOrder::Stored => 1u8,
        };
        let mut out = Vec::with_capacity(PAYLOAD_LEN);
        out.extend_from_slice(&self.idset.to_le_bytes());
        out.push(order);
        out.push(u8::from(scan.is_some()));
        let (a, b) = scan.unwrap_or((0, 0));
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
        out.extend_from_slice(&self.stretch.to_le_bytes());
        out
    }

    /// A payload that opened but does not parse is refused like one that did not open: it can
    /// only be a payload of another format.
    pub(super) fn decode(payload: &[u8]) -> Result<ItemsCursor> {
        if payload.len() != PAYLOAD_LEN || payload[5] > 1 {
            return Err(EngineError::CursorRefused);
        }
        let u32_at = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().expect("4"));
        let u64_at = |at: usize| u64::from_le_bytes(payload[at..at + 8].try_into().expect("8"));
        let idset = u32_at(0);
        let scan = (payload[5] == 1).then(|| (u32_at(6), u64_at(10)));
        let order = match payload[4] {
            0 => RecordsOrder::Map,
            1 => RecordsOrder::Stored,
            _ => return Err(EngineError::CursorRefused),
        };
        Ok(ItemsCursor {
            idset,
            position: Position { order, scan },
            stretch: u32_at(18),
        })
    }
}

/// An artifacts cursor's sealed payload: the idset, and the last `(level, ordinal)` the read has
/// returned or passed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ArtifactsCursor {
    pub(super) idset: u32,
    pub(super) scan: Option<(u32, u32)>,
}

/// `idset, has_scan, level, ordinal`, little-endian.
const ARTIFACTS_PAYLOAD_LEN: usize = 4 + 1 + 4 + 4;

impl ArtifactsCursor {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ARTIFACTS_PAYLOAD_LEN);
        out.extend_from_slice(&self.idset.to_le_bytes());
        out.push(u8::from(self.scan.is_some()));
        let (level, ordinal) = self.scan.unwrap_or((0, 0));
        out.extend_from_slice(&level.to_le_bytes());
        out.extend_from_slice(&ordinal.to_le_bytes());
        out
    }

    /// As [`ItemsCursor::decode`], a payload that opened and does not parse is refused.
    pub(super) fn decode(payload: &[u8]) -> Result<ArtifactsCursor> {
        if payload.len() != ARTIFACTS_PAYLOAD_LEN || payload[4] > 1 {
            return Err(EngineError::CursorRefused);
        }
        let u32_at = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().expect("4"));
        Ok(ArtifactsCursor {
            idset: u32_at(0),
            scan: (payload[4] == 1).then(|| (u32_at(5), u32_at(9))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f";

    fn binding(view: &str, incarnation: u64, hash: u8) -> Binding<'_> {
        Binding {
            route: Route::Items,
            view,
            incarnation,
            auth_data_hash: [hash; 32],
            layer: None,
        }
    }

    #[test]
    fn a_cursor_opens_only_under_the_binding_it_was_sealed_with() {
        let key = CursorKey::of(KEY);
        let cursor = ItemsCursor {
            idset: 7,
            position: Position {
                order: RecordsOrder::Map,
                scan: Some((4, u64::MAX)),
            },
            stretch: 16_384,
        };
        let token = key.seal(&binding("s0", 0, 1), &cursor.encode());
        let opened = key.open(&binding("s0", 0, 1), &token).unwrap();
        assert_eq!(ItemsCursor::decode(&opened).unwrap(), cursor);

        for other in [binding("s1", 0, 1), binding("s0", 1, 1), binding("s0", 0, 2)] {
            assert!(matches!(
                key.open(&other, &token),
                Err(EngineError::CursorRefused)
            ));
        }
        let rotated = CursorKey::of("0f0e0d0c0b0a09080706050403020100");
        assert!(matches!(
            rotated.open(&binding("s0", 0, 1), &token),
            Err(EngineError::CursorRefused)
        ));
    }

    /// An artifacts cursor opens only for its layer's name, entity and named level, and never
    /// under an items binding for the same view and credential.
    #[test]
    fn an_artifacts_cursor_opens_only_for_its_layer_and_level() {
        let key = CursorKey::of(KEY);
        let artifacts = |name, entity, level| Binding {
            route: Route::Artifacts,
            view: "s0",
            incarnation: 0,
            auth_data_hash: [1; 32],
            layer: Some(LayerBinding {
                name,
                entity,
                level,
            }),
        };
        let cursor = ArtifactsCursor {
            idset: 3,
            scan: Some((1, 40)),
        };
        let token = key.seal(&artifacts("clusters/a", 9, Some(1)), &cursor.encode());
        let opened = key.open(&artifacts("clusters/a", 9, Some(1)), &token).unwrap();
        assert_eq!(ArtifactsCursor::decode(&opened).unwrap(), cursor);
        for other in [
            artifacts("clusters/b", 9, Some(1)),
            artifacts("clusters/a", 8, Some(1)),
            artifacts("clusters/a", 9, None),
            artifacts("clusters/a", 9, Some(0)),
            binding("s0", 0, 1),
        ] {
            assert!(matches!(
                key.open(&other, &token),
                Err(EngineError::CursorRefused)
            ));
        }
    }

    #[test]
    fn two_cursors_for_one_position_share_no_text() {
        let key = CursorKey::of(KEY);
        let payload = ItemsCursor {
            idset: 1,
            position: Position {
                order: RecordsOrder::Stored,
                scan: Some((12, 0)),
            },
            stretch: 4096,
        }
        .encode();
        let a = key.seal(&binding("s0", 0, 1), &payload);
        let b = key.seal(&binding("s0", 0, 1), &payload);
        assert_ne!(a, b);
    }
}
