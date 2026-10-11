//! A Roaring view over bytes another value keeps alive, such as a read-only file mapping.

use std::any::Any;
use std::ops::Deref;
use std::sync::Arc;

use croaring::{Bitmap, BitmapView, Frozen, Portable};

/// A bitmap read in place from bytes inside an allocation `owner` keeps alive.
///
/// The view borrows the bytes for as long as this value lives, which the type system cannot
/// state, so the one constructor of each format is `unsafe` and the fields are private.
pub struct OwnedView {
    /// Declared first, so it is dropped before the owner of the bytes it reads.
    view: BitmapView<'static>,
    bytes: &'static [u8],
    format: Format,
    owner: Arc<dyn Any + Send + Sync>,
}

#[derive(Clone, Copy)]
enum Format {
    Portable,
    Frozen,
}

/// `Bitmap` carries croaring's own `Send` and `Sync`, and a view over bytes nothing may write is
/// no weaker: every read is a read of a `roaring_bitmap_t` and of the immutable bytes behind it,
/// and nothing writes through a view. The owner is `Send + Sync` by its bound.
unsafe impl Send for OwnedView {}
unsafe impl Sync for OwnedView {}

impl OwnedView {
    /// The portable-format bitmap in `bytes`, or `None` where they are not one.
    ///
    /// # Safety
    ///
    /// `bytes` must lie inside an allocation `owner` keeps alive, unwritten and unmoved, for as
    /// long as `owner` lives. A read-only file mapping, and an `Arc` over one, keeps that.
    pub unsafe fn portable(bytes: &[u8], owner: Arc<dyn Any + Send + Sync>) -> Option<OwnedView> {
        // The view's deserialiser is unchecked, and would read bytes that are not a bitmap as
        // containers at whatever the header claimed.
        Bitmap::try_deserialize::<Portable>(bytes)?;
        // SAFETY: the bytes deserialise, and the caller's contract is this one's.
        Some(unsafe { OwnedView::new(Format::Portable, bytes, owner) })
    }

    /// The frozen-format bitmap in `bytes`, or `None` where they are not one: not 32-byte aligned,
    /// or a length the header and its containers do not add up to.
    ///
    /// # Safety
    ///
    /// [`OwnedView::portable`]'s.
    pub unsafe fn frozen(bytes: &[u8], owner: Arc<dyn Any + Send + Sync>) -> Option<OwnedView> {
        // croaring's view panics where CRoaring refuses the layout, so CRoaring is asked first.
        // SAFETY: `roaring_bitmap_frozen_view` reads only inside `bytes`, and answers either null
        // or a header it allocated, which nothing else holds and which is freed here.
        unsafe {
            let probe =
                croaring_sys::roaring_bitmap_frozen_view(bytes.as_ptr().cast(), bytes.len());
            if probe.is_null() {
                return None;
            }
            croaring_sys::roaring_bitmap_free(probe);
        }
        // SAFETY: CRoaring accepts the layout, and the caller's contract is this one's.
        Some(unsafe { OwnedView::new(Format::Frozen, bytes, owner) })
    }

    /// # Safety
    ///
    /// `bytes` hold a bitmap in `format` that croaring's view accepts, and the caller's contract
    /// is [`OwnedView::portable`]'s.
    unsafe fn new(format: Format, bytes: &[u8], owner: Arc<dyn Any + Send + Sync>) -> OwnedView {
        // SAFETY: `owner` keeps the bytes for as long as it lives, and it is moved into the value
        // that holds the view, which drops the view first.
        let bytes: &'static [u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(bytes) };
        OwnedView {
            view: unsafe { view_of(format, bytes) },
            bytes,
            format,
            owner,
        }
    }
}

/// # Safety
///
/// `bytes` hold a bitmap in `format` that croaring's view accepts.
unsafe fn view_of(format: Format, bytes: &[u8]) -> BitmapView<'_> {
    unsafe {
        match format {
            Format::Portable => BitmapView::deserialize::<Portable>(bytes),
            Format::Frozen => BitmapView::deserialize::<Frozen>(bytes),
        }
    }
}

impl Deref for OwnedView {
    type Target = Bitmap;

    fn deref(&self) -> &Bitmap {
        &self.view
    }
}

impl Clone for OwnedView {
    /// A view of the same bytes, whose owner is shared.
    fn clone(&self) -> Self {
        OwnedView {
            // SAFETY: the bytes this value's own view reads, kept by the owner cloned beside it.
            view: unsafe { view_of(self.format, self.bytes) },
            bytes: self.bytes,
            format: self.format,
            owner: Arc::clone(&self.owner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bitmap` frozen into a buffer of its own, and where in the buffer the 32-byte aligned bytes
    /// lie.
    fn frozen(bitmap: &Bitmap) -> (Arc<Vec<u8>>, std::ops::Range<usize>) {
        let mut buffer = Vec::new();
        let bytes = bitmap.serialize_into_vec::<Frozen>(&mut buffer);
        let (at, len) = (bytes.as_ptr() as usize, bytes.len());
        let start = at - buffer.as_ptr() as usize;
        let range = start..start + len;
        (Arc::new(buffer), range)
    }

    #[test]
    fn a_whole_frozen_buffer_reads_as_its_bitmap_and_a_clone_as_the_same() {
        let bitmap = Bitmap::of(&[1, 5, 70_000, 1 << 31]);
        let (buffer, range) = frozen(&bitmap);
        let view = unsafe { OwnedView::frozen(&buffer[range], buffer.clone()) }.expect("frozen");
        assert_eq!(*view, bitmap);
        assert_eq!(*view.clone(), bitmap);
    }

    #[test]
    fn a_cut_or_misaligned_frozen_buffer_is_refused() {
        let bitmap = Bitmap::of(&[1, 5, 70_000, 1 << 31]);
        let (buffer, range) = frozen(&bitmap);
        for end in [range.start, range.start + 3, range.end - 1] {
            let cut = unsafe { OwnedView::frozen(&buffer[range.start..end], buffer.clone()) };
            assert!(cut.is_none(), "cut to {} bytes", end - range.start);
        }
        // The same bytes one byte past a 32-byte boundary.
        let mut shifted = vec![0u8; range.len() + 64];
        let at = (33 - shifted.as_ptr() as usize % 32) % 32;
        shifted[at..at + range.len()].copy_from_slice(&buffer[range.clone()]);
        let shifted = Arc::new(shifted);
        let bytes = &shifted[at..at + range.len()];
        assert_eq!(bytes.as_ptr() as usize % 32, 1);
        assert!(unsafe { OwnedView::frozen(bytes, shifted.clone()) }.is_none());
    }

    #[test]
    fn bytes_that_are_not_a_portable_bitmap_are_refused() {
        let bitmap = Bitmap::of(&[3, 9, 1 << 20]);
        let owner = Arc::new(bitmap.serialize::<Portable>());
        let view = unsafe { OwnedView::portable(&owner, owner.clone()) }.expect("portable");
        assert_eq!(*view, bitmap);
        let cut = unsafe { OwnedView::portable(&owner[..owner.len() - 1], owner.clone()) };
        assert!(cut.is_none());
    }
}
