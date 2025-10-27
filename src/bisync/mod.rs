// Bidirectional synchronization
//
// Enables two-way sync with conflict detection and resolution.

pub mod classifier;
pub mod engine;
pub mod resolver;
pub mod state;

pub use classifier::{classify_changes, Change, ChangeType};
pub use engine::{BisyncEngine, BisyncOptions};
#[allow(unused_imports)]
pub(crate) use engine::{BisyncResult, BisyncStats, ConflictInfo};
pub use resolver::{
    conflict_filename, resolve_changes, ConflictResolution, ResolvedChanges, SyncAction,
};
pub use state::{BisyncStateDb, Side, SyncState};
