pub mod dict;
pub mod postings;

pub use dict::{Dict, DictWriter};
pub use postings::{write_postings, PostingRef, PostingsReader};
