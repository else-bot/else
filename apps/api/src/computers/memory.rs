//! Owner-approved memory sizes for a bot's computer. The chosen size is
//! kept as `chosen_memory_mib`, so every later boot (`configure_boot`) keeps
//! it instead of the configured default. Applying a size stops a running
//! computer (its jobs end, its disk stays), sets its boot size, and starts
//! it again.

use super::{Acquire, Busy, ComputerError, ComputerManager, Desired, Observed, Reason};
use uuid::Uuid;

/// The memory sizes a bot may ask for, in MiB.
pub const MEMORY_SIZES_MIB: [u32; 3] = [2048, 4096, 8192];

/// A computer's memory and the larger sizes it may be given now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySizes {
    pub current_mib: u32,
    /// The configured size, which the owner can return it to.
    pub default_mib: u32,
    /// Larger sizes that fit the host's computer budget beside the other
    /// running computers.
    pub offered_mib: Vec<u32>,
}

/// A size applied to a computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resized {
    pub memory_mib: u32,
    /// It was running and started again at the new size.
    pub restarted: bool,
}

/// Size in GiB for messages: `4 GiB`, `1.5 GiB`.
pub fn gib(mib: u32) -> String {
    if mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{:.1} GiB", f64::from(mib) / 1024.0)
    }
}

impl ComputerManager {
    /// The bot's computer memory and the sizes it could be raised to.
    pub async fn memory_sizes(&self, bot_id: Uuid) -> Result<MemorySizes, ComputerError> {
        let mut conn = self.pool.acquire().await?;
        let row = Self::row(&mut conn, bot_id).await?;
        let (cpus, current) = row
            .as_ref()
            .map_or((self.config.cpus, self.config.memory_mib), |row| {
                (row.cpus as u32, row.memory_mib as u32)
            });
        let mut offered_mib = Vec::new();
        for size in MEMORY_SIZES_MIB.into_iter().filter(|size| *size > current) {
            let (_, max_memory) = self.boot_size(Some(size));
            if self.admits(&mut conn, bot_id, cpus, max_memory).await? {
                offered_mib.push(size);
            }
        }
        Ok(MemorySizes {
            current_mib: current,
            default_mib: self.config.memory_mib,
            offered_mib,
        })
    }

    /// Boot the bot's computer with `memory_mib` from now on (`None`: the
    /// configured size). A running computer stops, ending its jobs and
    /// commands, and starts again at the new size; a stopped one takes it on
    /// its next start. The choice is recorded before the computer stops, so
    /// an interrupted resize finishes at the next start. Applying the
    /// current size again changes nothing.
    pub async fn resize_memory(
        &self,
        bot_id: Uuid,
        memory_mib: Option<u32>,
    ) -> Result<Resized, ComputerError> {
        let target = memory_mib.unwrap_or(self.config.memory_mib);
        if target != self.config.memory_mib && !MEMORY_SIZES_MIB.contains(&target) {
            return Err(ComputerError::Refused(format!(
                "{} is not an offered size",
                gib(target)
            )));
        }
        if self.upgrading(bot_id).await {
            return Err(ComputerError::Busy(Busy::Upgrading));
        }
        let op = self.op(bot_id).await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
                .bind(bot_id)
                .fetch_optional(&mut *tx)
                .await?;
        if active != Some(true) {
            return Err(ComputerError::Refused("this bot is not active".into()));
        }
        let row = self.ensure_row(&mut tx, bot_id).await?;
        if row.chosen_memory_mib.map(|m| m as u32) == memory_mib && row.memory_mib as u32 == target
        {
            return Ok(Resized {
                memory_mib: target,
                restarted: false,
            });
        }
        let (_, max_memory) = self.boot_size(memory_mib);
        if target > row.memory_mib as u32
            && !self
                .admits(&mut tx, bot_id, row.cpus as u32, max_memory)
                .await?
        {
            return Err(ComputerError::Busy(Busy::NoRoomFor(target)));
        }
        let restarts = match row.observed_state {
            Observed::Running | Observed::Paused | Observed::Error => {
                row.desired_state == Desired::Auto
            }
            Observed::Stopped | Observed::Absent => false,
            Observed::Missing => {
                return Err(ComputerError::Refused(
                    "this bot's computer is missing from the runtime".into(),
                ));
            }
            Observed::Creating
            | Observed::Starting
            | Observed::Pausing
            | Observed::Resuming
            | Observed::Stopping => {
                return Err(ComputerError::Busy(Busy::ChangingState));
            }
        };
        // A computer never created yet is created at the new size.
        sqlx::query(
            "UPDATE bot_computers SET chosen_memory_mib = ?2,
                 memory_mib = CASE WHEN observed_state = 'absent' THEN ?3 ELSE memory_mib END,
                 max_memory_mib = CASE WHEN observed_state = 'absent' THEN ?4 ELSE max_memory_mib END
             WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .bind(memory_mib.map(|m| m as i32))
        .bind(target as i32)
        .bind(max_memory as i32)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if row.observed_state == Observed::Absent {
            return Ok(Resized {
                memory_mib: target,
                restarted: false,
            });
        }
        if matches!(
            row.observed_state,
            Observed::Running | Observed::Paused | Observed::Error
        ) && !self.stop_held(bot_id, Reason::Resize).await?
        {
            return Err(ComputerError::Busy(Busy::DidNotStop));
        }
        let row = {
            let mut conn = self.pool.acquire().await?;
            Self::row(&mut conn, bot_id).await?.expect("exists")
        };
        let row = self.configure_boot(bot_id, &row).await?;
        if row.memory_mib as u32 != target {
            return Err(ComputerError::Refused(format!(
                "the computer runtime could not set {}; it boots at {} until a later start \
                 applies it",
                gib(target),
                gib(row.memory_mib as u32)
            )));
        }
        drop(op);
        if !restarts {
            return Ok(Resized {
                memory_mib: target,
                restarted: false,
            });
        }
        let holder = format!("resize:{}", Uuid::new_v4());
        match self.acquire(bot_id, &holder).await? {
            Acquire::Ready { lease_id, .. } => {
                self.release(lease_id).await?;
                Ok(Resized {
                    memory_mib: target,
                    restarted: true,
                })
            }
            other => {
                let why = match other {
                    Acquire::Queued { reason }
                    | Acquire::Unavailable { reason }
                    | Acquire::DesktopFailed { reason } => reason,
                    Acquire::OwnerStopped => "the owner stopped it".into(),
                    Acquire::BotInactive => "the bot is not active".into(),
                    Acquire::Ready { .. } => unreachable!("matched above"),
                };
                Err(ComputerError::Busy(Busy::NotRestarted {
                    memory_mib: target,
                    why,
                }))
            }
        }
    }

    /// The boot memory, and the memory it may grow to, for a computer with
    /// `chosen` memory (`None`: the configured size).
    pub(super) fn boot_size(&self, chosen: Option<u32>) -> (u32, u32) {
        let memory = chosen.unwrap_or(self.config.memory_mib);
        let max = self
            .config
            .growth()
            .map_or(memory, |(_, grown)| grown.max(memory));
        (memory, max)
    }
}
