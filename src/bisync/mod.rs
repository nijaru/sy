// Bidirectional synchronization
//
// Enables two-way sync with conflict detection and resolution.
// WIP: not yet wired into sync flow. Module is intentionally stubbed.

#[allow(dead_code)]
pub mod classifier;
#[allow(dead_code)]
pub mod engine;
#[allow(dead_code)]
pub mod lock;
#[allow(dead_code)]
pub mod resolver;
#[allow(dead_code)]
pub mod state;

pub use classifier::{classify_changes, Change, ChangeType};
#[allow(unused_imports)] // Used by binary crate
pub use engine::{BisyncEngine, BisyncOptions};
pub use lock::SyncLock;
pub use resolver::{
    conflict_filename, resolve_changes, ConflictResolution, ResolvedChanges, SyncAction,
};
pub use state::{BisyncStateDb, Side, SyncState};
