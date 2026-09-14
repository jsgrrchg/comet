//! Local RDP client. No GUI, engine, cloud, or native clipboard dependencies.
//! Creating configuration never starts a connection; a session has one owner.

mod model;
pub use model::*;
