use crate::*;
use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub trait Clock {
    fn now_unix_ms(&self) -> Result<u64>;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_unix_ms(&self) -> Result<u64> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::new(ErrorCode::ClockError, e.to_string()))?;
        u64::try_from(duration.as_millis())
            .map_err(|_| Error::new(ErrorCode::ClockError, "clock overflow"))
    }
}
pub struct Invocation<'a> {
    pub request: &'a WorkRequest,
    pub effective_deadline_unix_ms: u64,
}
/// Synchronous adapters must cooperate with deadlines. Isolation/preemption belongs
/// to a process/remote adapter; this trait cannot safely kill an arbitrary Rust call.
pub trait CapabilityAdapter: Send + Sync {
    fn descriptor(&self) -> CapabilityDescriptor;
    fn invoke(&self, invocation: Invocation<'_>) -> AdapterOutcome;
}
struct Registration {
    capability: Capability,
    adapter: Box<dyn CapabilityAdapter>,
}
#[derive(Default)]
pub struct Worker {
    adapters: BTreeMap<(String, String), Registration>,
}
impl Worker {
    pub fn register(&mut self, adapter: impl CapabilityAdapter + 'static) -> Result<()> {
        let capability = Capability::new(adapter.descriptor())?;
        let r = &capability.descriptor().capability;
        let key = (r.id.clone(), r.version.clone());
        if self.adapters.contains_key(&key) {
            return Err(Error::new(
                ErrorCode::DuplicateCapability,
                "capability ID/version is already registered",
            ));
        }
        self.adapters.insert(
            key,
            Registration {
                capability,
                adapter: Box::new(adapter),
            },
        );
        Ok(())
    }
    pub fn capabilities(&self) -> Vec<&Capability> {
        self.adapters.values().map(|a| &a.capability).collect()
    }
    pub fn capability(&self, id: &str, version: &str) -> Result<&Capability> {
        self.adapters
            .get(&(id.into(), version.into()))
            .map(|a| &a.capability)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MissingCapability,
                    "exact capability ID/version is unavailable",
                )
            })
    }
    pub fn execute(&self, request: &WorkRequest, grant: &ExecutionGrant) -> Result<AcceptedResult> {
        self.execute_with_clock(request, grant, &SystemClock)
    }
    pub fn execute_with_clock(
        &self,
        request: &WorkRequest,
        grant: &ExecutionGrant,
        clock: &impl Clock,
    ) -> Result<AcceptedResult> {
        request.validate_shape()?;
        let registration = self
            .adapters
            .get(&(
                request.capability.id.clone(),
                request.capability.version.clone(),
            ))
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::MissingCapability,
                    "exact capability ID/version is unavailable",
                )
            })?;
        let capability = &registration.capability;
        let received = clock.now_unix_ms()?;
        let admission_timer = Instant::now();
        validate_request(request, grant, capability, received)?;
        let started = clock.now_unix_ms()?;
        if started < received {
            return Err(Error::new(
                ErrorCode::ClockError,
                "clock moved backwards during request validation",
            ));
        }
        if started >= request.deadline_unix_ms
            || admission_timer.elapsed().as_millis()
                >= u128::from(request.deadline_unix_ms - received)
        {
            return Err(Error::new(
                ErrorCode::Expired,
                "request expired during validation; adapter was not invoked",
            ));
        }
        let deadline = request
            .deadline_unix_ms
            .min(started.saturating_add(capability.descriptor().timeout_ms));
        let monotonic = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            registration.adapter.invoke(Invocation {
                request,
                effective_deadline_unix_ms: deadline,
            })
        }))
        .map_err(|_| Error::new(ErrorCode::AdapterPanicked, "read-only adapter panicked"))?;
        let completed = clock.now_unix_ms()?;
        if completed < started {
            return Err(Error::new(
                ErrorCode::ClockError,
                "clock moved backwards during invocation",
            ));
        }
        if completed >= deadline
            || monotonic.elapsed().as_millis() >= u128::from(deadline - started)
            || admission_timer.elapsed().as_millis()
                >= u128::from(request.deadline_unix_ms - received)
        {
            return Err(Error::new(
                ErrorCode::DeadlineExceeded,
                "adapter result arrived after the effective deadline",
            ));
        }
        let result = WorkResult {
            protocol_version: PROTOCOL_VERSION,
            request_digest: digest(request)?,
            completed_at_unix_ms: completed,
            outcome,
        };
        let accepted = accept_result(request, grant, capability, result, completed)?;
        let checked = clock.now_unix_ms()?;
        if checked < completed {
            return Err(Error::new(
                ErrorCode::ClockError,
                "clock moved backwards during result validation",
            ));
        }
        if checked >= deadline
            || monotonic.elapsed().as_millis() >= u128::from(deadline - started)
            || admission_timer.elapsed().as_millis()
                >= u128::from(request.deadline_unix_ms - received)
        {
            return Err(Error::new(
                ErrorCode::DeadlineExceeded,
                "result validation exceeded the effective deadline",
            ));
        }
        Ok(accepted)
    }
    /// Same checks as in-process invocation; the grant arrives from the host,
    /// never from the untrusted request document.
    pub fn dispatch_json(&self, request: &[u8], grant: &ExecutionGrant) -> Result<Vec<u8>> {
        let request: WorkRequest = parse_message(request)?;
        let result = self.execute(&request, grant)?;
        to_message(result.result())
    }
}
