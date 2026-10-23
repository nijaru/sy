//! Initial root creation is new-only work, admitted after the full LOCAL plan.
use super::*;
use crate::rooted_fs::local_source_effects::{native_object, LocalSourceEffects};

#[derive(Default)]
pub(super) enum CreationState {
    #[default]
    Undeclared,
    Declared(CreationProof),
    Admitted(CreationProof),
}

impl std::fmt::Debug for CreationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Undeclared => "Undeclared",
            Self::Declared(_) => "Declared",
            Self::Admitted(_) => "Admitted",
        })
    }
}

pub(super) struct CreationProof {
    source: LocalSourceEffects,
    ancestor: native_object::NativeObjectKey,
    namespace: namespace::Shared,
}

impl CreationProof {
    fn same_scope(&self, source: &LocalSourceEffects, rooted: &RootedFs) -> bool {
        self.source.same_source(source)
            && rooted
                .destination_namespace
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &self.namespace))
    }
}

impl PendingRoot {
    pub(in crate::rooted_fs) fn declare_source_effect_blocking(
        &self,
        effects: &LocalSourceEffects,
        path: &Path,
        kind: SourceEffectKind,
    ) -> Result<()> {
        self.ancestor.verify_root_path_blocking()?;
        if !matches!(
            kind,
            SourceEffectKind::LeafPublication | SourceEffectKind::DirectoryNamespaceChange
        ) {
            return effects.refuse_native_effect_blocking(path);
        }
        let prefix: PathBuf = self.missing.iter().collect();
        // The authenticated first absent edge qualifies new-only descendants.
        // No OLD, parent object identity or name profile is projected below it.
        effects.validate_declared_effect_blocking(&self.ancestor, &prefix.join(path), kind)?;
        let ancestor = native_object::object_key(self.ancestor.root_fd.as_raw_fd(), None)?
            .ok_or_else(|| RootedFsError::SourceEffectUnqualified(self.path.clone()))?;
        let namespace = self
            .ancestor
            .destination_namespace
            .as_ref()
            .ok_or_else(|| RootedFsError::NamespaceUnqualified(self.path.clone()))?;
        let mut state = self
            .source_creation
            .lock()
            .map_err(|_| RootedFsError::NamespaceCatalogueTerminal)?;
        match &*state {
            CreationState::Undeclared => {
                *state = CreationState::Declared(CreationProof {
                    source: effects.clone(),
                    ancestor,
                    namespace: Arc::clone(namespace),
                });
            }
            CreationState::Declared(proof)
                if proof.same_scope(effects, &self.ancestor) && proof.ancestor == ancestor => {}
            _ => return Err(RootedFsError::SourceEffectUnqualified(self.path.clone())),
        }
        Ok(())
    }

    pub(in crate::rooted_fs) fn complete_source_effects(
        &self,
        effects: &LocalSourceEffects,
    ) -> Result<()> {
        let mut state = self
            .source_creation
            .lock()
            .map_err(|_| RootedFsError::NamespaceCatalogueTerminal)?;
        match &*state {
            CreationState::Undeclared => return Ok(()),
            CreationState::Declared(proof) if proof.same_scope(effects, &self.ancestor) => {}
            _ => return Err(RootedFsError::SourceEffectUnqualified(self.path.clone())),
        }
        // Undeclared is fail-closed if unwinding interrupts this move. Only the
        // completed-plan boundary can turn genuine declarations into admission.
        *state = match std::mem::take(&mut *state) {
            CreationState::Declared(proof) => CreationState::Admitted(proof),
            state => state,
        };
        Ok(())
    }

    fn require_completed_source_effects(&self) -> Result<()> {
        let Some(effects) = &self.ancestor.local_source_effects else {
            return Ok(());
        };
        effects.validate_originals_blocking()?;
        let ancestor = {
            let state = self
                .source_creation
                .lock()
                .map_err(|_| RootedFsError::NamespaceCatalogueTerminal)?;
            match &*state {
                CreationState::Admitted(proof) if proof.same_scope(effects, &self.ancestor) => {
                    proof.ancestor
                }
                _ => return Err(RootedFsError::SourceEffectUnqualified(self.path.clone())),
            }
        };
        if native_object::object_key(self.ancestor.root_fd.as_raw_fd(), None)? != Some(ancestor) {
            return Err(RootedFsError::SourceEffectUnqualified(self.path.clone()));
        }
        Ok(())
    }
}

impl PendingRoot {
    pub(super) fn acquire_blocking(self, local: bool) -> Result<RootedFs> {
        self.acquire_checked(local, None)
    }

    pub(super) fn acquire_checked(
        self,
        local: bool,
        backup_address: Option<&Path>,
    ) -> Result<RootedFs> {
        self.ancestor.require_writable()?;
        self.require_completed_source_effects()?;
        let mut fd = self.ancestor.root_fd.try_clone()?;
        let mut prefix = PathBuf::new();
        for name in &self.missing {
            if local {
                self.ancestor.verify_root_path_blocking()?;
            }
            prefix.push(name);
            self.ancestor.verify_parent_binding_blocking(&prefix, &fd)?;
            if stat_at_optional(fd.as_raw_fd(), name)?.is_some() {
                return Err(RootedFsError::DestinationChanged(self.path));
            }
            let name_c = component_cstring(name)?;
            self.ancestor
                .namespace_predicts_absent(&prefix, fd.as_raw_fd())?;
            if let Some(address) = backup_address {
                self.ancestor.verify_backup_address_blocking(address)?;
            }
            // Completed declarations qualify new-only work, not this future
            // parent's identity. Repeat native admission on each actual parent.
            let _permit = self.ancestor.admit_mutation_blocking(SourceEffect {
                kind: SourceEffectKind::DirectoryNamespaceChange,
                parent: fd.as_raw_fd(),
                name,
                object: None,
                path: &prefix,
            })?;
            self.ancestor.verify_parent_binding_blocking(&prefix, &fd)?;
            if let Some(address) = backup_address {
                self.ancestor.verify_backup_address_blocking(address)?;
            }
            // SAFETY: fd is held, name_c is one live NUL-terminated component.
            if unsafe { libc::mkdirat(fd.as_raw_fd(), name_c.as_ptr(), 0o777) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let observed = stat_at_optional(fd.as_raw_fd(), name)?
                .ok_or_else(|| RootedFsError::DestinationChanged(self.path.clone()))?;
            let child = open_dir_at(fd.as_raw_fd(), name)?;
            let opened = stat_fd(child.as_raw_fd())?;
            if observed.st_dev != opened.st_dev
                || observed.st_ino != opened.st_ino
                || observed.st_mode & libc::S_IFMT != libc::S_IFDIR
            {
                return Err(RootedFsError::DestinationChanged(self.path));
            }
            self.ancestor.advance_namespace_directory(
                &prefix,
                fd.as_raw_fd(),
                child.as_raw_fd(),
            )?;
            fd = child;
        }
        if local {
            self.ancestor.verify_root_path_blocking()?;
        }
        // Observational initial acquisition, not atomic creator ownership.
        let (parent, leaf) = self.ancestor.open_parent_blocking(&prefix)?;
        let current = open_dir_at(parent.as_raw_fd(), &leaf)?;
        let observed = stat_fd(current.as_raw_fd())?;
        let held = stat_fd(fd.as_raw_fd())?;
        if observed.st_dev != held.st_dev || observed.st_ino != held.st_ino {
            return Err(RootedFsError::DestinationChanged(self.path));
        }
        let mut rooted = RootedFs::from_operand_fd(self.path, fd);
        rooted.mutation_admission = self.ancestor.mutation_admission;
        rooted.run_admission = self.ancestor.run_admission;
        rooted.local_source_effects = self.ancestor.local_source_effects;
        rooted.retirement = self.ancestor.retirement;
        #[cfg(test)]
        {
            rooted.mutation_pause = self.ancestor.mutation_pause;
        }
        rooted.destination_namespace = self.ancestor.destination_namespace;
        rooted.namespace_qualification_required = self.ancestor.namespace_qualification_required;
        rooted.namespace_prefix = self.ancestor.namespace_prefix.join(prefix);
        Ok(rooted)
    }
}
