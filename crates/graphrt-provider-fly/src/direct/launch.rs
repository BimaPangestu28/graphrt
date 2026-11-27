//! Updating a Machine's config and getting it running.
//!
//! On Fly, `POST /machines/{id}` with `skip_launch: false` reboots a running
//! Machine with the new config, but a **stopped** Machine stays stopped (its
//! event is `update stopped`). A standby version, a paused deployment and a
//! scale-to-zero Machine the proxy stopped while idle are all stopped, so a
//! rollback, a resume or a redeploy of the Machine with the store would wait
//! for a start that never comes. After the update, a Machine that is not
//! running is started explicitly.
//!
//! For a few seconds after an update, Fly reports the Machine as `replacing`
//! and still shows its **previous** config. A resume right after a pause would
//! read the Machine as not paused, so an update that changes what graphrt
//! reads back (pause, demote) waits until the Machine is no longer
//! `replacing`.

use std::time::Instant;

use graphrt_provider_api::ProviderErrorKind;

use super::{DirectError, ErrorCode, FlyDirect};
use crate::model::{Machine, MachineState, UpdateMachineRequest};

impl FlyDirect {
    /// Replaces the Machine's config and makes sure it boots with it (does not
    /// wait for the boot). With `skip_launch`, only the config is replaced.
    ///
    /// @param machine_id - the Machine (&str)
    /// @param update - the new config (&UpdateMachineRequest)
    /// @throws DirectError - the update or the start was refused
    pub(super) async fn update_and_launch(
        &self,
        machine_id: &str,
        update: &UpdateMachineRequest<'_>,
    ) -> Result<(), DirectError> {
        self.client.update_machine(machine_id, update).await?;
        let updated = self.settle(machine_id).await?;
        if update.skip_launch || !updated.state.is_startable() {
            return Ok(());
        }
        match self.client.start_machine(machine_id).await {
            Ok(()) => Ok(()),
            // Fly may have begun starting it in the meantime: that is what we want.
            Err(error) if error.kind != ProviderErrorKind::Transient => {
                let fresh = self.client.get_machine(machine_id).await?;
                if fresh.state.is_startable() { Err(error.into()) } else { Ok(()) }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Replaces the config of a Machine that stays down (`skip_launch`) and
    /// waits until Fly shows the new config.
    ///
    /// @throws DirectError - the update was refused, or Fly kept the Machine `replacing`
    pub(super) async fn update_stopped(
        &self,
        machine_id: &str,
        update: &UpdateMachineRequest<'_>,
    ) -> Result<(), DirectError> {
        self.client.update_machine(machine_id, update).await?;
        self.settle(machine_id).await.map(drop)
    }

    /// Polls the Machine until it is no longer `replacing`.
    ///
    /// @returns The Machine as Fly shows it once the update is applied
    /// @throws DirectError(StartTimeout) - still `replacing` after the start timeout
    async fn settle(&self, machine_id: &str) -> Result<Machine, DirectError> {
        let deadline = Instant::now() + self.settings.start_timeout;
        loop {
            let machine = self.client.get_machine(machine_id).await?;
            if machine.state != MachineState::Replacing {
                return Ok(machine);
            }
            if Instant::now() >= deadline {
                return Err(DirectError::new(
                    ErrorCode::StartTimeout,
                    format!(
                        "Machine {machine_id} was still taking its new config after {} s",
                        self.settings.start_timeout.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(self.settings.poll_interval).await;
        }
    }
}
