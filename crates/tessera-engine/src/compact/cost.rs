/// The process's resident set in bytes: total, anonymous, file-backed. Files written through a
/// mapping land in the file-backed figure, reclaimable once written back; spool buffers and term
/// encodes are anonymous and are not, so the split shows which part is growing.
fn resident_set() -> (u64, u64, u64) {
    let r = tessera_types::process::resident_bytes();
    (r.total, r.anon, r.file)
}

/// What one pass cost: its wall clock, and the process's resident set at the moment it ended.
/// Sampled at pass boundaries, giving attribution (which pass the resident set climbed during)
/// rather than a true peak.
#[derive(Debug, Clone, Copy)]
pub struct PassCost {
    pub pass: &'static str,
    pub elapsed: std::time::Duration,
    /// Total and anonymous resident bytes at the end of the pass.
    pub rss: u64,
    pub anon: u64,
}

/// A staircase under construction: the rows recorded so far and the instant the next row is
/// measured from. The fold thread starts one for its passes; `publish_fold` resumes it for the
/// publication's phases, so `/control/status` reports the fold end to end.
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

    /// Continue a staircase another thread recorded: `cost` is its rows and `mark` is when its
    /// last row ended, so the first row recorded here covers the hand-off.
    pub(crate) fn resume(cost: Vec<PassCost>, mark: std::time::Instant) -> Self {
        Self { cost, mark }
    }

    /// Close one row: the wall clock since the previous row ended, and the resident set now.
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

    /// When the last recorded row ended.
    pub(crate) fn mark(&self) -> std::time::Instant {
        self.mark
    }

    pub(crate) fn into_cost(self) -> Vec<PassCost> {
        self.cost
    }
}
