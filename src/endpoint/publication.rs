use std::sync::atomic::{AtomicUsize, Ordering};

const CLOSED: usize = 1 << (usize::BITS - 1);
const COUNT: usize = CLOSED - 1;

/// Linearizes staged publication admission against cancellation without holding
/// a lock across native I/O. Closing never waits for admitted work: its completion
/// can remain uncertain to a disconnected peer or a dropped awaiting future.
#[derive(Debug, Default)]
pub(crate) struct PublicationAdmission {
    state: AtomicUsize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    Closed,
    Exhausted,
}

impl PublicationAdmission {
    pub(crate) fn close(&self) {
        self.state.fetch_or(CLOSED, Ordering::AcqRel);
    }

    pub(crate) fn admit(&self) -> Result<PublicationPermit<'_>, AdmissionError> {
        // Both operations modify this same atomic word. A successful increment
        // precedes closure or observes it and fails; separate snapshot checks
        // cannot establish that ordering. No filesystem state is protected here.
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & CLOSED != 0 {
                return Err(AdmissionError::Closed);
            }
            if state & COUNT == COUNT {
                return Err(AdmissionError::Exhausted);
            }
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(PublicationPermit { admission: self }),
                Err(current) => state = current,
            }
        }
    }
}

pub(crate) struct PublicationPermit<'a> {
    admission: &'a PublicationAdmission,
}

impl Drop for PublicationPermit<'_> {
    fn drop(&mut self) {
        // The count cannot underflow: only a successful increment creates a
        // permit. Subtraction preserves the closed bit, including the last drop.
        self.admission.state.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_does_not_wait_for_permits_or_reopen_after_their_release() {
        let admission = PublicationAdmission::default();
        let first = admission.admit().unwrap();
        let second = admission.admit().unwrap();
        admission.close();
        assert!(matches!(admission.admit(), Err(AdmissionError::Closed)));
        drop((first, second));
        admission.close();
        assert!(matches!(admission.admit(), Err(AdmissionError::Closed)));
    }

    #[test]
    fn count_exhaustion_is_not_closure_or_wrapping_admission() {
        let admission = PublicationAdmission {
            state: AtomicUsize::new(COUNT),
        };
        assert!(matches!(admission.admit(), Err(AdmissionError::Exhausted)));
        admission.close();
        assert!(matches!(admission.admit(), Err(AdmissionError::Closed)));
    }
}
