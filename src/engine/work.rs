use super::scheduler::ResourceRequest;

/// One concrete execution action paired with the scheduler resources it may
/// consume while running.
///
/// Semantic planning remains separate from admission control: backends lower a
/// `SyncOp` into their own action type, attach a bounded `ResourceRequest`, then
/// hand the resulting work item to the shared scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem<T> {
    action: T,
    resources: ResourceRequest,
}

impl<T> WorkItem<T> {
    pub const fn new(action: T, resources: ResourceRequest) -> Self {
        Self { action, resources }
    }

    pub const fn resources(&self) -> ResourceRequest {
        self.resources
    }

    pub const fn action(&self) -> &T {
        &self.action
    }

    pub fn into_action(self) -> T {
        self.action
    }

    pub fn into_parts(self) -> (T, ResourceRequest) {
        (self.action, self.resources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_item_keeps_action_and_admission_request_together() {
        let resources = ResourceRequest {
            active_files: 1,
            buffered_bytes: 4 * 1024 * 1024,
            metadata_ops: 0,
            cpu_tasks: 1,
            network_writes: 1,
        };
        let item = WorkItem::new("file", resources);

        assert_eq!(item.action(), &"file");
        assert_eq!(item.resources(), resources);
        assert_eq!(item.into_parts(), ("file", resources));
    }
}
