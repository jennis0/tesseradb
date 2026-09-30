/// Resident bytes: total, anonymous, file-backed. Files written through a mapping are
/// file-backed and can be reclaimed once written back; anonymous memory cannot.
fn resident_set() -> (u64, u64, u64) {
    let r = tessera_types::process::resident_bytes();
    (r.total, r.anon, r.file)
}

/// One pass's wall clock and the resident set when it ended. Sampled at pass boundaries, so it
/// shows which pass the memory grew in but misses the peak within a pass.
#[derive(Debug, Clone, Copy)]
pub struct PassCost {
    pub pass: &'static str,
    pub elapsed: std::time::Duration,
    pub rss: u64,
    pub anon: u64,
}

/// Per-pass cost rows. `publish_fold` resumes the fold thread's staircase for the publication,
/// so status reports the fold end to end.
pub(crate) struct Staircase {
    cost: Vec<PassCost>,
    mark: std::time::Instant,
}

impl Staircase {
    pub(crate) fn start() -> Self {
        Self {
            cost: Vec::with_capacity(14),
            mark: std::time::Instant::now(),
        }
    }

    /// `mark` is when the other thread's last row ended, so the first row here covers the hand-off.
    pub(crate) fn resume(cost: Vec<PassCost>, mark: std::time::Instant) -> Self {
        Self { cost, mark }
    }

    /// Closes a row: the wall clock since the previous row ended, and the resident set now.
    pub(crate) fn record(&mut self, pass: &'static str) {
        let (rss, anon, _) = resident_set();
        self.cost.push(PassCost {
            pass,
            elapsed: self.mark.elapsed(),
            rss,
            anon,
        });
        self.mark = std::time::Instant::now();
    }

    pub(crate) fn mark(&self) -> std::time::Instant {
        self.mark
    }

    pub(crate) fn into_cost(self) -> Vec<PassCost> {
        self.cost
    }
}
