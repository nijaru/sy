//! Complete LOCAL declaration preflight. Scratch contains destination facts,
//! never SOURCE permission. Public/native mutations do not consume this state.
use super::*;
use std::sync::Mutex;
mod records;
#[cfg(all(test, target_os = "macos"))]
mod tests;
pub(in crate::rooted_fs) use records::Declarations;
use records::{invalid, Record, Snapshot};

#[derive(Clone, Copy)]
pub(crate) enum EffectRoot {
    Main,
    Backup,
}

pub(crate) struct EffectPreflight {
    effects: LocalSourceEffects,
    state: Arc<Mutex<State>>,
}

struct State {
    main: operand::DestinationRoot,
    backup: Option<std::result::Result<operand::DestinationRoot, Arc<RootedFsError>>>,
    declarations: Declarations,
    collecting: bool,
}

impl State {
    fn root(&self, root: EffectRoot) -> Result<operand::DestinationRoot> {
        if matches!(root, EffectRoot::Main) {
            return Ok(self.main.clone());
        }
        match self.backup.as_ref().ok_or_else(invalid)? {
            Ok(root) => Ok(root.clone()),
            Err(error) => Err(RootedFsError::BackupRootCapture(Arc::clone(error))),
        }
    }
}

impl EffectPreflight {
    pub(crate) async fn new(
        effects: &LocalSourceEffects,
        main: &operand::DestinationRoot,
        backup: Option<&backup::BackupRoot>,
    ) -> Result<Self> {
        let effects = effects.clone();
        let main = main.clone();
        // Retain optional capture errors without making an unused root an effect.
        let backup = backup.map(|root| root.original().map_err(Arc::new));
        tokio::task::spawn_blocking(move || {
            effects
                .inventory
                .validate_preflight_scratch_blocking()
                .map_err(source_error)?;
            Ok(Self {
                effects,
                state: Arc::new(Mutex::new(State {
                    main,
                    backup,
                    declarations: Declarations::new()?,
                    collecting: true,
                })),
            })
        })
        .await
        .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }

    pub(crate) async fn declare(
        &self,
        root: EffectRoot,
        path: RelativePath,
        kind: SourceEffectKind,
    ) -> Result<()> {
        let state = Arc::clone(&self.state);
        let effects = self.effects.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock().map_err(|_| invalid())?;
            if !state.collecting {
                return Err(invalid());
            }
            state.collecting = false;
            effects.validate_lifetime_blocking()?;
            let destination = match state.root(root) {
                Ok(root) => root,
                Err(error) => {
                    effects.validate_originals_blocking()?;
                    return Err(error);
                }
            };
            let snapshot = capture(&effects, &destination, path.as_path(), kind)?;
            if let Some(snapshot) = snapshot {
                state.declarations.append(Record {
                    root,
                    path,
                    kind,
                    snapshot,
                })?;
            }
            state.collecting = true;
            Ok(())
        })
        .await
        .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }

    pub(crate) async fn finish(self) -> Result<()> {
        let effects = self.effects;
        let state = self.state;
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock().map_err(|_| invalid())?;
            if !state.collecting {
                return Err(invalid());
            }
            state.collecting = false;
            if state.declarations.count == 0 {
                return effects.validate_lifetime_blocking();
            }
            if !effects
                .inventory
                .excludes_declarations_blocking(&mut state.declarations)
                .map_err(source_error)?
            {
                let ordinal = state.declarations.refused.unwrap_or(0);
                let record = state.declarations.read(ordinal)?;
                return Err(RootedFsError::SourceEffectUnqualified(
                    record.path.into_path_buf(),
                ));
            }
            // No FD-per-declaration retention: re-open through the same original
            // namespace and authenticate the exact captured native footprint.
            for ordinal in 0..state.declarations.count {
                effects.validate_lifetime_blocking()?;
                let record = state.declarations.read(ordinal)?;
                let root = state.root(record.root)?;
                let snapshot = capture(&effects, &root, record.path.as_path(), record.kind)?;
                if snapshot.as_ref() != Some(&record.snapshot) {
                    effects.validate_originals_blocking()?;
                    return Err(RootedFsError::DestinationChanged(
                        record.path.into_path_buf(),
                    ));
                }
            }
            effects.validate_lifetime_blocking()
        })
        .await
        .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }
}

fn capture(
    effects: &LocalSourceEffects,
    destination: &operand::DestinationRoot,
    path: &Path,
    kind: SourceEffectKind,
) -> Result<Option<Snapshot>> {
    let validate = |effect: SourceEffect<'_>| {
        let footprint = effect.capture_footprint()?;
        if footprint.empty_directory.is_some() {
            // Empty OLD retirement keeps its genuine pin through its own fresh
            // SOURCE census and late EOF/binding checks. It is not a stored claim.
            return Ok(effects
                .validate_footprint_blocking(&effect, &footprint)
                .map(|()| None));
        }
        Ok(Ok(Some(Snapshot::capture(&effect, footprint))))
    };
    let captured = match destination {
        operand::DestinationRoot::Present(rooted) => rooted
            .verify_root_path_blocking()
            .and_then(|()| rooted.with_declared_source_effect_blocking(path, kind, validate)),
        operand::DestinationRoot::Pending(pending) => {
            pending.with_declared_source_effect_blocking(effects, path, kind, validate)
        }
    };
    match captured {
        Ok(source_checked) => source_checked,
        Err(destination_error) => {
            // Root/namespace walks and native capture have not checked SOURCE.
            // Keep its error priority without rechecking an already failed
            // empty-OLD census and masking that first typed SOURCE error.
            effects.validate_originals_blocking()?;
            Err(destination_error)
        }
    }
}
