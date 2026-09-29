//! Content-addressed immutable object store.
//!
//! Objects are whole files identified by [`ObjectId`] (BLAKE3 of plaintext
//! bytes). Layout under the store root:
//!
//! ```text
//! <root>/objects/<hex[0..2]>/<hex[2..4]>/<full 64-char hex>
//! <root>/tmp/            (in-flight writes)
//! ```

mod error;
mod store;

pub use error::StoreError;
pub use store::{ObjectStore, PutOutcome, SweepReport};
