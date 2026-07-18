//! Durable record persistence for the router — the generic [`sovds::recorder`]
//! under the router's historical name. `FileRecorder` is `sovds::recorder::Recorder`;
//! `append(&record)` infers the router domain from the record type.

pub use sovds::recorder::{suffixed, RecordError, Recorder as FileRecorder};
