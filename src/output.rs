// SPDX-License-Identifier: GPL-3.0-or-later

use std::{cell::RefCell, num::NonZeroU32};

use pinnacle_api_defs::pinnacle::signal::v0alpha1::{OutputMoveResponse, OutputResizeResponse};
use smithay::{
    desktop::layer_map_for_output,
    output::{Mode, Output, Scale},
    reexports::calloop::LoopHandle,
    utils::{Logical, Point, Transform},
    wayland::session_lock::LockSurface,
};

use crate::{
    focus::WindowKeyboardFocusStack,
    layout::transaction::{LayoutTransaction, SnapshotTarget},
    protocol::screencopy::Screencopy,
    state::{Pinnacle, State, WithState},
    tag::Tag,
    window::window_state::FloatingOrTiled,
};

/// A unique identifier for an output.
///
/// An empty string represents an invalid output.
// TODO: maybe encode that in the type
#[derive(Debug, Hash, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct OutputName(pub String);

impl OutputName {
    /// Get the output with this name.
    pub fn output(&self, pinnacle: &Pinnacle) -> Option<Output> {
        pinnacle
            .space
            .outputs()
            .find(|output| output.name() == self.0)
            .cloned()
    }
}

/// State of an output's blanking status for session lock.
#[derive(Debug, Default, Copy, Clone)]
pub enum BlankingState {
    /// The output is not blanked and is displaying normal content.
    #[default]
    NotBlanked,
    /// A blank frame has been queued up.
    Blanking,
    /// A blank frame has been displayed.
    Blanked,
}

/// The state of an output
#[derive(Default, Debug)]
pub struct OutputState {
    pub tags: Vec<Tag>,
    pub focus_stack: WindowKeyboardFocusStack,
    pub screencopy: Option<Screencopy>,
    pub serial: Option<NonZeroU32>,
    pub modes: Vec<Mode>,
    pub lock_surface: Option<LockSurface>,
    pub blanking_state: BlankingState,
    /// A pending layout transaction.
    pub layout_transaction: Option<LayoutTransaction>,
}

impl WithState for Output {
    type State = OutputState;

    fn with_state<F, T>(&self, func: F) -> T
    where
        F: FnOnce(&Self::State) -> T,
    {
        let state = self
            .user_data()
            .get_or_insert(RefCell::<Self::State>::default);

        func(&state.borrow())
    }

    fn with_state_mut<F, T>(&self, func: F) -> T
    where
        F: FnOnce(&mut Self::State) -> T,
    {
        let state = self
            .user_data()
            .get_or_insert(RefCell::<Self::State>::default);

        func(&mut state.borrow_mut())
    }
}

impl OutputState {
    pub fn focused_tags(&self) -> impl Iterator<Item = &Tag> {
        self.tags.iter().filter(|tag| tag.active())
    }

    pub fn new_wait_layout_transaction(
        &mut self,
        loop_handle: LoopHandle<'static, State>,
        fullscreen_and_up_snapshots: impl IntoIterator<Item = SnapshotTarget>,
        under_fullscreen_snapshots: impl IntoIterator<Item = SnapshotTarget>,
    ) {
        if let Some(ts) = self.layout_transaction.as_mut() {
            ts.wait();
        } else {
            self.layout_transaction = Some(LayoutTransaction::new_and_wait(
                loop_handle,
                fullscreen_and_up_snapshots,
                under_fullscreen_snapshots,
            ));
        }
    }
}

impl Pinnacle {
    /// A wrapper around [`Output::change_current_state`] that additionally sends an output
    /// geometry signal.
    pub fn change_output_state(
        &mut self,
        output: &Output,
        mode: Option<Mode>,
        transform: Option<Transform>,
        scale: Option<Scale>,
        location: Option<Point<i32, Logical>>,
    ) {
        let old_scale = output.current_scale().fractional_scale();

        output.change_current_state(mode, transform, scale, location);
        if let Some(location) = location {
            self.space.map_output(output, location);
            self.signal_state.output_move.signal(|buf| {
                buf.push_back(OutputMoveResponse {
                    output_name: Some(output.name()),
                    x: Some(location.x),
                    y: Some(location.y),
                });
            });
        }
        if mode.is_some() || transform.is_some() || scale.is_some() {
            layer_map_for_output(output).arrange();
            self.signal_state.output_resize.signal(|buf| {
                let geo = self.space.output_geometry(output);
                buf.push_back(OutputResizeResponse {
                    output_name: Some(output.name()),
                    logical_width: geo.map(|geo| geo.size.w as u32),
                    logical_height: geo.map(|geo| geo.size.h as u32),
                });
            });
        }
        if let Some(mode) = mode {
            output.set_preferred(mode);
            output.with_state_mut(|state| state.modes.push(mode));
        }

        if let Some(scale) = scale {
            let pos_multiplier = old_scale / scale.fractional_scale();

            for win in self
                .windows
                .iter()
                .filter(|win| win.output(self).as_ref() == Some(output))
                .filter(|win| win.with_state(|state| state.floating_or_tiled.is_floating()))
                .cloned()
                .collect::<Vec<_>>()
            {
                let Some(output) = win.output(self) else { unreachable!() };

                let output_loc = output.current_location();

                // FIXME: get everything out of this with_state
                win.with_state_mut(|state| {
                    let FloatingOrTiled::Floating(rect) = &mut state.floating_or_tiled else {
                        unreachable!()
                    };

                    let loc = rect.loc;

                    let mut loc_relative_to_output = loc - output_loc;
                    loc_relative_to_output = loc_relative_to_output
                        .to_f64()
                        .upscale(pos_multiplier)
                        .to_i32_round();

                    rect.loc = loc_relative_to_output + output_loc;
                    self.space.map_element(win.clone(), rect.loc, false);
                });
            }
        }

        if let Some(lock_surface) = output.with_state(|state| state.lock_surface.clone()) {
            lock_surface.with_pending_state(|state| {
                let Some(new_geo) = self.space.output_geometry(output) else {
                    return;
                };
                state.size = Some((new_geo.size.w as u32, new_geo.size.h as u32).into());
            });

            lock_surface.send_configure();
        }
    }
}
