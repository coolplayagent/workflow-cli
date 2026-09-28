use super::*;

impl Engine {
    /// Pause/resume are journaled controls, never a sampled host decision.
    pub(super) fn lifecycle(&mut self, event: &EventKind) -> Result<bool> {
        let (reason, pausing) = match event {
            EventKind::Pause { reason } => (reason, true),
            EventKind::Resume { reason } => (reason, false),
            _ => return Ok(false),
        };
        if self.state.status != RunStatus::Running
            || pausing == self.state.pause.is_some()
            || reason.trim().is_empty()
            || reason.len() > 1024
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "pause/resume requires a running run, a state change and a nonempty reason of at most 1024 bytes",
            ));
        }
        self.state.pause = pausing.then(|| Pause {
            reason: reason.clone(),
            at_unix_ms: self.state.now_unix_ms,
        });
        Ok(true)
    }
}
