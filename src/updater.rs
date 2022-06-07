use crate::traits::{FirmwareDevice, UpdateService};
use drogue_ajour_protocol::{Command, Status};
use embedded_hal_async::delay::DelayUs;
use heapless::Vec;

/// The error types that the updater may return during the update process.
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error<D, S> {
    Encode,
    Decode,
    Delay,
    Device(D),
    Service(S),
}

/// The device status as determined after running the updater.
#[derive(PartialEq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum DeviceStatus {
    Synced,
    Updated,
}

struct UpdaterState {
    current_version: Vec<u8, 32>,
    next_offset: u32,
    next_version: Option<Vec<u8, 32>>,
}

/// The updater process that uses the update service to perform a firmware update check
/// for a device. If the device needs to be updated, the updater will follow the update protocol
pub struct FirmwareUpdater<T>
where
    T: UpdateService,
{
    service: T,
}

impl<T> FirmwareUpdater<T>
where
    T: UpdateService,
{
    /// Create a new instance of the updater with the provided service instance.
    pub fn new(service: T) -> Self {
        Self { service }
    }

    async fn check<F: FirmwareDevice, D: DelayUs>(
        &mut self,
        device: &mut F,
        delay: &mut D,
    ) -> Result<bool, Error<F::Error, T::Error>> {
        let mut state = {
            let initial = device.status().await.map_err(|e| Error::Device(e))?;
            UpdaterState {
                current_version: Vec::from_slice(initial.current_version)
                    .map_err(|_| Error::Encode)?,
                next_offset: initial.next_offset,
                next_version: if let Some(next_version) = &initial.next_version {
                    Some(Vec::from_slice(next_version).map_err(|_| Error::Encode)?)
                } else {
                    None
                },
            }
        };

        #[allow(unused_mut)]
        #[allow(unused_assignments)]
        #[allow(renamed_and_removed_lints)]
        #[allow(mutable_borrow_reservation_conflict)]
        loop {
            let status = if let Some(next) = &state.next_version {
                Status::update(
                    &state.current_version,
                    Some(F::MTU as u32),
                    state.next_offset,
                    next,
                    None,
                )
            } else {
                Status::first(&state.current_version, Some(F::MTU as u32), None)
            };

            debug!("Sending status: {:?}", status);

            let cmd = self
                .service
                .request(&status)
                .await;
            match cmd {
                Ok(Command::Write {
                    version,
                    offset,
                    data,
                    correlation_id: _,
                }) => {
                    if offset == 0 {
                        debug!(
                            "Updating device firmware from {} to {}",
                            state.current_version,
                            version.as_ref()
                        );
                        device
                            .start(version.as_ref())
                            .await
                            .map_err(|e| Error::Device(e))?;
                    }
                    device
                        .write(offset, data.as_ref())
                        .await
                        .map_err(|e| Error::Device(e))?;
                    state.next_offset += data.len() as u32;
                    state
                        .next_version
                        .replace(Vec::from_slice(version.as_ref()).map_err(|_| Error::Decode)?);
                }
                Ok(Command::Sync {
                    version: _,
                    poll: _,
                    correlation_id: _,
                }) => {
                    debug!("Device firmware is up to date");
                    device.synced().await.map_err(|e| Error::Device(e))?;
                    return Ok(true);
                }
                Ok(Command::Wait {
                    poll,
                    correlation_id: _,
                }) => {
                    debug!("Instruction to wait for {:?} seconds", poll);
                    if let Some(poll) = poll {
                        delay
                            .delay_ms(poll * 1000)
                            .await
                            .map_err(|_| Error::Delay)?;
                    }
                }
                Ok(Command::Swap {
                    version,
                    checksum,
                    correlation_id: _,
                }) => {
                    debug!("Swaping firmware");
                    device
                        .update(version.as_ref(), checksum.as_ref())
                        .await
                        .map_err(|e| Error::Device(e))?;
                    return Ok(false);
                }
                Err(e) => {
                    #[cfg(feature = "defmt")]
                    debug!("Error reporting status: {:?}", defmt::Debug2Format(&e));
                    #[cfg(not(feature = "defmt"))]
                    debug!("Error reporting status: {:?}", e);
                }
            }
        }
    }

    /// Run the firmware update protocol. The update is finished with two outcomes:
    ///
    /// 1) The device is in sync, in which case `DeviceStatus::Synced` is returned.
    /// 2) The device is updated, in which case `DeviceStatus::Updated` is returned. It is the responsibility
    ///    of called to reset the device in order to run the new firmware.
    pub async fn run<F: FirmwareDevice, D: DelayUs>(
        &mut self,
        device: &mut F,
        delay: &mut D,
    ) -> Result<DeviceStatus, Error<F::Error, T::Error>> {
        if self.check(device, delay).await? {
            Ok(DeviceStatus::Synced)
        } else {
            Ok(DeviceStatus::Updated)
        }
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use core::future::Future;

    use crate::DeviceStatus;
    use crate::FirmwareUpdater;
    use crate::InMemory;
    use crate::Simulator;

    pub struct TokioDelay;

    impl embedded_hal_async::delay::DelayUs for TokioDelay {
        type Error = Infallible;

        type DelayUsFuture<'a> = impl Future<Output = Result<(), Self::Error>>
        where
            Self: 'a;

        fn delay_us(&mut self, us: u32) -> Self::DelayUsFuture<'_> {
            async move {
                tokio::time::sleep(tokio::time::Duration::from_micros(us as u64)).await;
                Ok(())
            }
        }

        type DelayMsFuture<'a> = impl Future<Output = Result<(), Self::Error>>
        where
            Self: 'a;

        fn delay_ms(&mut self, ms: u32) -> Self::DelayMsFuture<'_> {
            async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(ms as u64)).await;
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn test_update_protocol_synced() {
        let service = InMemory::new(b"1", &[1; 1024]);
        let mut device = Simulator::new(b"1");

        let mut updater = FirmwareUpdater::new(service);
        let status = updater.run(&mut device, &mut TokioDelay).await.unwrap();
        assert_eq!(status, DeviceStatus::Synced);
    }

    #[tokio::test]
    async fn test_update_protocol_updated() {
        let service = InMemory::new(b"2", &[1; 1024]);
        let mut device = Simulator::new(b"1");

        let mut updater = FirmwareUpdater::new(service);
        let status = updater.run(&mut device, &mut TokioDelay).await.unwrap();
        assert_eq!(status, DeviceStatus::Updated);
    }
}
