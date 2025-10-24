// Bidirectional synchronization
//
// Enables two-way sync with conflict detection and resolution.

pub mod classifier;
pub mod engine;
pub mod resolver;
pub mod state;

pub use classifier::{Change, ChangeType, classify_changes};
pub use engine::{BisyncEngine, BisyncOptions, BisyncResult, BisyncStats, ConflictInfo};
pub use resolver::{conflict_filename, resolve_changes, ConflictResolution, ResolvedChanges, SyncAction};
pub use state::{BisyncStateDb, Side, SyncState};
