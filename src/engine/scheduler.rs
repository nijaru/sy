use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Byte permits are quantized to keep Tokio's `u32` multi-permit API practical
/// while still bounding resident/in-flight memory closely.
pub const BYTE_QUANTUM: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceBudget {
    pub active_files: u32,
    pub buffered_bytes: u64,
    pub metadata_ops: u32,
    pub cpu_tasks: u32,
    pub network_writes: u32,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            active_files: 8,
            buffered_bytes: 64 * 1024 * 1024,
            metadata_ops: 32,
            cpu_tasks: 4,
            network_writes: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceRequest {
    /// Number of concurrently active file jobs represented by this work item.
    pub active_files: u32,
    /// Maximum bytes this work item may keep buffered or in flight at once.
    /// This is deliberately not the logical file size.
    pub buffered_bytes: u64,
    pub metadata_ops: u32,
    pub cpu_tasks: u32,
    pub network_writes: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("scheduler budget {resource} must be greater than zero")]
    ZeroBudget { resource: &'static str },

    #[error(
        "scheduler byte budget is too large to represent: {bytes} bytes with {quantum}-byte permits"
    )]
    ByteBudgetTooLarge { bytes: u64, quantum: u64 },

    #[error("resource request exceeds {resource} budget: requested {requested}, budget {budget}")]
    RequestTooLarge {
        resource: &'static str,
        requested: u64,
        budget: u64,
    },

    #[error("scheduler was closed while acquiring {resource}")]
    Closed { resource: &'static str },
}

/// Shared scheduler admission controller.
///
/// All resource classes are acquired in one fixed order, preventing circular
/// waits between work items that need more than one class. The returned permit
/// is RAII-owned; dropping it releases every acquired resource.
#[derive(Clone)]
pub struct Scheduler {
    budget: ResourceBudget,
    byte_units: u32,
    active_files: Arc<Semaphore>,
    buffered_bytes: Arc<Semaphore>,
    metadata_ops: Arc<Semaphore>,
    cpu_tasks: Arc<Semaphore>,
    network_writes: Arc<Semaphore>,
}

impl Scheduler {
    pub fn new(budget: ResourceBudget) -> Result<Self, SchedulerError> {
        validate_nonzero("active_files", budget.active_files)?;
        validate_nonzero("buffered_bytes", budget.buffered_bytes)?;
        validate_nonzero("metadata_ops", budget.metadata_ops)?;
        validate_nonzero("cpu_tasks", budget.cpu_tasks)?;
        validate_nonzero("network_writes", budget.network_writes)?;

        let byte_units = byte_units(budget.buffered_bytes)?;
        Ok(Self {
            budget,
            byte_units,
            active_files: Arc::new(Semaphore::new(budget.active_files as usize)),
            buffered_bytes: Arc::new(Semaphore::new(byte_units as usize)),
            metadata_ops: Arc::new(Semaphore::new(budget.metadata_ops as usize)),
            cpu_tasks: Arc::new(Semaphore::new(budget.cpu_tasks as usize)),
            network_writes: Arc::new(Semaphore::new(budget.network_writes as usize)),
        })
    }

    pub const fn budget(&self) -> ResourceBudget {
        self.budget
    }

    pub async fn acquire(&self, request: ResourceRequest) -> Result<ResourcePermit, SchedulerError> {
        self.validate_request(request)?;
        let requested_byte_units = request_byte_units(request.buffered_bytes)?;

        // Keep this order stable. A single global acquisition order prevents
        // work items from deadlocking by holding different resource classes.
        let active_files = acquire(
            Arc::clone(&self.active_files),
            request.active_files,
            "active_files",
        )
        .await?;
        let buffered_bytes = acquire(
            Arc::clone(&self.buffered_bytes),
            requested_byte_units,
            "buffered_bytes",
        )
        .await?;
        let metadata_ops = acquire(
            Arc::clone(&self.metadata_ops),
            request.metadata_ops,
            "metadata_ops",
        )
        .await?;
        let cpu_tasks = acquire(
            Arc::clone(&self.cpu_tasks),
            request.cpu_tasks,
            "cpu_tasks",
        )
        .await?;
        let network_writes = acquire(
            Arc::clone(&self.network_writes),
            request.network_writes,
            "network_writes",
        )
        .await?;

        Ok(ResourcePermit {
            _active_files: active_files,
            _buffered_bytes: buffered_bytes,
            _metadata_ops: metadata_ops,
            _cpu_tasks: cpu_tasks,
            _network_writes: network_writes,
        })
    }

    fn validate_request(&self, request: ResourceRequest) -> Result<(), SchedulerError> {
        validate_request(
            "active_files",
            request.active_files as u64,
            self.budget.active_files as u64,
        )?;
        validate_request(
            "buffered_bytes",
            request.buffered_bytes,
            self.budget.buffered_bytes,
        )?;
        validate_request(
            "metadata_ops",
            request.metadata_ops as u64,
            self.budget.metadata_ops as u64,
        )?;
        validate_request(
            "cpu_tasks",
            request.cpu_tasks as u64,
            self.budget.cpu_tasks as u64,
        )?;
        validate_request(
            "network_writes",
            request.network_writes as u64,
            self.budget.network_writes as u64,
        )?;

        let requested_units = request_byte_units(request.buffered_bytes)?;
        if requested_units > self.byte_units {
            return Err(SchedulerError::RequestTooLarge {
                resource: "buffered_bytes",
                requested: request.buffered_bytes,
                budget: self.budget.buffered_bytes,
            });
        }
        Ok(())
    }
}

/// Holds scheduler capacity until the admitted work item completes.
pub struct ResourcePermit {
    _active_files: Option<OwnedSemaphorePermit>,
    _buffered_bytes: Option<OwnedSemaphorePermit>,
    _metadata_ops: Option<OwnedSemaphorePermit>,
    _cpu_tasks: Option<OwnedSemaphorePermit>,
    _network_writes: Option<OwnedSemaphorePermit>,
}

async fn acquire(
    semaphore: Arc<Semaphore>,
    permits: u32,
    resource: &'static str,
) -> Result<Option<OwnedSemaphorePermit>, SchedulerError> {
    if permits == 0 {
        return Ok(None);
    }
    semaphore
        .acquire_many_owned(permits)
        .await
        .map(Some)
        .map_err(|_| SchedulerError::Closed { resource })
}

fn validate_nonzero(resource: &'static str, value: impl Into<u64>) -> Result<(), SchedulerError> {
    if value.into() == 0 {
        return Err(SchedulerError::ZeroBudget { resource });
    }
    Ok(())
}

fn validate_request(
    resource: &'static str,
    requested: u64,
    budget: u64,
) -> Result<(), SchedulerError> {
    if requested > budget {
        return Err(SchedulerError::RequestTooLarge {
            resource,
            requested,
            budget,
        });
    }
    Ok(())
}

fn request_byte_units(bytes: u64) -> Result<u32, SchedulerError> {
    if bytes == 0 {
        return Ok(0);
    }
    byte_units(bytes)
}

fn byte_units(bytes: u64) -> Result<u32, SchedulerError> {
    let units = bytes.div_ceil(BYTE_QUANTUM);
    u32::try_from(units).map_err(|_| SchedulerError::ByteBudgetTooLarge {
        bytes,
        quantum: BYTE_QUANTUM,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn budget() -> ResourceBudget {
        ResourceBudget {
            active_files: 2,
            buffered_bytes: 2 * BYTE_QUANTUM,
            metadata_ops: 2,
            cpu_tasks: 1,
            network_writes: 1,
        }
    }

    #[test]
    fn rejects_zero_and_oversized_budgets() {
        let mut invalid = budget();
        invalid.active_files = 0;
        assert!(matches!(
            Scheduler::new(invalid),
            Err(SchedulerError::ZeroBudget {
                resource: "active_files"
            })
        ));

        let invalid = ResourceBudget {
            buffered_bytes: (u32::MAX as u64 + 1) * BYTE_QUANTUM,
            ..budget()
        };
        assert!(matches!(
            Scheduler::new(invalid),
            Err(SchedulerError::ByteBudgetTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_work_that_cannot_fit_budget() {
        let scheduler = Scheduler::new(budget()).unwrap();
        let error = scheduler
            .acquire(ResourceRequest {
                active_files: 3,
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SchedulerError::RequestTooLarge {
                resource: "active_files",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn byte_budget_applies_backpressure_and_releases_on_drop() {
        let scheduler = Scheduler::new(budget()).unwrap();
        let first = scheduler
            .acquire(ResourceRequest {
                buffered_bytes: BYTE_QUANTUM + 1,
                ..Default::default()
            })
            .await
            .unwrap();

        let second_scheduler = scheduler.clone();
        let second = tokio::spawn(async move {
            second_scheduler
                .acquire(ResourceRequest {
                    buffered_bytes: 1,
                    ..Default::default()
                })
                .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!second.is_finished());
        drop(first);
        assert!(tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn resource_classes_are_independently_bounded() {
        let scheduler = Scheduler::new(budget()).unwrap();
        let first = scheduler
            .acquire(ResourceRequest {
                cpu_tasks: 1,
                ..Default::default()
            })
            .await
            .unwrap();

        let metadata = scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                ..Default::default()
            })
            .await
            .unwrap();

        let second_scheduler = scheduler.clone();
        let cpu = tokio::spawn(async move {
            second_scheduler
                .acquire(ResourceRequest {
                    cpu_tasks: 1,
                    ..Default::default()
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!cpu.is_finished());
        drop(metadata);
        assert!(!cpu.is_finished());
        drop(first);
        assert!(tokio::time::timeout(Duration::from_secs(1), cpu)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }
}
