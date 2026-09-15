//! Bridge a device's physical controller (paired to the phone, read there
//! with the Gamepad API) onto a virtual Xbox 360 controller on this PC, via
//! [ViGEmBus](https://github.com/ViGEm/ViGEmBus).
//!
//! `ClientMessage::GamepadState`'s fields are already laid out like
//! `XINPUT_GAMEPAD`, so there's nothing to remap here — just hand it to the
//! virtual pad. ViGEmBus is a separate one-time driver install; without it,
//! every state update is a no-op after one attempt (and one notice to the
//! device explaining why).

use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

pub struct GamepadHub {
    pad: Option<Xbox360Wired<Client>>,
    /// Set after the first connect attempt, success or failure, so a phone
    /// polling at 60Hz doesn't retry a missing driver every frame.
    tried: bool,
}

impl GamepadHub {
    pub fn new() -> Self {
        Self { pad: None, tried: false }
    }

    /// Feed one controller frame to the virtual pad. Returns `Some(notice)`
    /// the first time (and only the first time) it can't — worth surfacing to
    /// the device, since a controller that silently does nothing looks like a
    /// bug rather than a missing driver.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        buttons: u16,
        left_trigger: u8,
        right_trigger: u8,
        thumb_lx: i16,
        thumb_ly: i16,
        thumb_rx: i16,
        thumb_ry: i16,
    ) -> Option<String> {
        let notice = self.ensure_pad();
        if let Some(pad) = &mut self.pad {
            let gamepad = XGamepad {
                buttons: XButtons { raw: buttons },
                left_trigger,
                right_trigger,
                thumb_lx,
                thumb_ly,
                thumb_rx,
                thumb_ry,
            };
            if let Err(e) = pad.update(&gamepad) {
                tracing::debug!(error = ?e, "controller update failed");
            }
        }
        notice
    }

    /// Unplug the virtual pad — the device's controller was disconnected or
    /// the tab lost focus of it. Reconnecting later plugs in a fresh one.
    pub fn disconnect(&mut self) {
        self.pad = None;
        self.tried = false;
    }

    fn ensure_pad(&mut self) -> Option<String> {
        if self.pad.is_some() || self.tried {
            return None;
        }
        self.tried = true;
        match Self::create() {
            Ok(pad) => {
                tracing::info!("virtual controller connected");
                self.pad = Some(pad);
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "virtual controller unavailable");
                Some(format!(
                    "Controller not connected on the PC: {e}. Install ViGEmBus (free, one-time) \
                     from github.com/ViGEm/ViGEmBus/releases, then reconnect."
                ))
            }
        }
    }

    fn create() -> anyhow::Result<Xbox360Wired<Client>> {
        let client = Client::connect()?;
        let mut pad = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
        pad.plugin()?;
        pad.wait_ready()?;
        Ok(pad)
    }
}

impl Default for GamepadHub {
    fn default() -> Self {
        Self::new()
    }
}
