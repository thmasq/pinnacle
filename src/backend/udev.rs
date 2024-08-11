// SPDX-License-Identifier: GPL-3.0-or-later

mod drm;
mod gamma;

use assert_matches::assert_matches;
pub use drm::util::drm_mode_from_api_modeline;
use indexmap::IndexSet;

use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{anyhow, ensure, Context};
use drm::{set_crtc_active, util::create_drm_mode};
use pinnacle_api_defs::pinnacle::signal::v0alpha1::OutputConnectResponse;
use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
            Buffer, Fourcc,
        },
        drm::{
            compositor::{DrmCompositor, PrimaryPlaneElement, RenderFrameResult},
            gbm::GbmFramebuffer,
            CreateDrmNodeError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmEventMetadata,
            DrmNode, NodeType,
        },
        egl::{self, context::ContextPriority, EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            self, damage,
            element::{self, surface::render_elements_from_surface_tree, Element, Id},
            gles::{GlesRenderbuffer, GlesRenderer},
            multigpu::{gbm::GbmGlesBackend, GpuManager, MultiRenderer},
            sync::SyncPoint,
            utils::{CommitCounter, DamageSet},
            Bind, Blit, BufferType, ExportMem, ImportDma, ImportEgl, ImportMemWl, Offscreen,
            Renderer, TextureFilter,
        },
        session::{
            self,
            libseat::{self, LibSeatSession},
            Session,
        },
        udev::{self, UdevBackend, UdevEvent},
        SwapBuffersError,
    },
    desktop::utils::OutputPresentationFeedback,
    input::pointer::CursorImageStatus,
    output::{Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            self, generic::Generic, timer::Timer, Dispatcher, Idle, Interest, LoopHandle,
            PostAction, RegistrationToken,
        },
        drm::control::{connector, crtc, ModeTypeFlags},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_protocols::wp::{
            linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1,
            presentation_time::server::wp_presentation_feedback,
        },
        wayland_server::{
            protocol::{wl_shm, wl_surface::WlSurface},
            DisplayHandle,
        },
    },
    utils::{DeviceFd, IsAlive, Point, Rectangle, Transform},
    wayland::{
        dmabuf::{self, DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
        shm::shm_format_to_fourcc,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, trace, warn};

use crate::{
    backend::Backend,
    config::ConnectorSavedState,
    output::{BlankingState, OutputMode, OutputName},
    render::{
        pointer::pointer_render_elements, take_presentation_feedback, OutputRenderElement,
        CLEAR_COLOR, CLEAR_COLOR_LOCKED,
    },
    state::{Pinnacle, State, SurfaceDmabufFeedback, WithState},
};

use self::drm::util::EdidInfo;

use super::{BackendData, UninitBackend};

const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const SUPPORTED_FORMATS_8BIT_ONLY: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

/// A [`MultiRenderer`] that uses the [`GbmGlesBackend`].
pub type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

type UdevRenderFrameResult<'a> =
    RenderFrameResult<'a, GbmBuffer, GbmFramebuffer, OutputRenderElement<UdevRenderer<'a>>>;

/// Udev state attached to each [`Output`].
#[derive(Debug, PartialEq)]
struct UdevOutputData {
    /// The GPU node
    device_id: DrmNode,
    /// The [Crtc][crtc::Handle] the output is pushing to
    crtc: crtc::Handle,
}

// TODO: document desperately
pub struct Udev {
    pub session: LibSeatSession,
    udev_dispatcher: Dispatcher<'static, UdevBackend, State>,
    display_handle: DisplayHandle,
    pub(super) dmabuf_state: Option<(DmabufState, DmabufGlobal)>,
    pub(super) primary_gpu: DrmNode,
    pub(super) gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    backends: HashMap<DrmNode, UdevBackendData>,

    pub(super) upscale_filter: TextureFilter,
    pub(super) downscale_filter: TextureFilter,
}

impl Backend {
    #[allow(dead_code)]
    fn udev(&self) -> &Udev {
        let Backend::Udev(udev) = self else { unreachable!() };
        udev
    }

    fn udev_mut(&mut self) -> &mut Udev {
        let Backend::Udev(udev) = self else { unreachable!() };
        udev
    }
}

impl Udev {
    pub(crate) fn try_new(display_handle: DisplayHandle) -> anyhow::Result<UninitBackend<Udev>> {
        // Initialize session
        let (session, notifier) = LibSeatSession::new()?;

        // Get the primary gpu
        let primary_gpu = udev::primary_gpu(session.seat())
            .context("unable to get primary gpu path")?
            .and_then(|x| {
                DrmNode::from_path(x)
                    .ok()?
                    .node_with_type(NodeType::Render)?
                    .ok()
            })
            .unwrap_or_else(|| {
                udev::all_gpus(session.seat())
                    .expect("failed to get gpu paths")
                    .into_iter()
                    .find_map(|x| DrmNode::from_path(x).ok())
                    .expect("No GPU!")
            });
        info!("Using {} as primary gpu", primary_gpu);

        let gpu_manager =
            GpuManager::new(GbmGlesBackend::with_context_priority(ContextPriority::High))?;

        // Initialize the udev backend
        let udev_backend = UdevBackend::new(session.seat())?;

        let udev_dispatcher = Dispatcher::new(udev_backend, move |event, _, state: &mut State| {
            let udev = state.backend.udev_mut();
            let pinnacle = &mut state.pinnacle;
            match event {
                // GPU connected
                UdevEvent::Added { device_id, path } => {
                    if let Err(err) = DrmNode::from_dev_id(device_id)
                        .map_err(DeviceAddError::DrmNode)
                        .and_then(|node| udev.device_added(pinnacle, node, &path))
                    {
                        error!("Skipping device {device_id}: {err}");
                    }
                }
                UdevEvent::Changed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        udev.device_changed(pinnacle, node)
                    }
                }
                // GPU disconnected
                UdevEvent::Removed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        udev.device_removed(pinnacle, node)
                    }
                }
            }
        });

        let mut udev = Udev {
            display_handle,
            udev_dispatcher,
            dmabuf_state: None,
            session,
            primary_gpu,
            gpu_manager,
            backends: HashMap::new(),

            upscale_filter: TextureFilter::Linear,
            downscale_filter: TextureFilter::Linear,
        };

        Ok(UninitBackend {
            seat_name: udev.seat_name(),
            init: Box::new(move |pinnacle| {
                pinnacle
                    .loop_handle
                    .register_dispatcher(udev.udev_dispatcher.clone())?;

                let things = udev
                    .udev_dispatcher
                    .as_source_ref()
                    .device_list()
                    .map(|(id, path)| (id, path.to_path_buf()))
                    .collect::<Vec<_>>();

                // Create DrmNodes from already connected GPUs
                for (device_id, path) in things {
                    if let Err(err) = DrmNode::from_dev_id(device_id)
                        .map_err(DeviceAddError::DrmNode)
                        .and_then(|node| udev.device_added(pinnacle, node, &path))
                    {
                        error!("Skipping device {device_id}: {err}");
                    }
                }

                // Initialize libinput backend
                let mut libinput_context = Libinput::new_with_udev::<
                    LibinputSessionInterface<LibSeatSession>,
                >(udev.session.clone().into());
                libinput_context
                    .udev_assign_seat(pinnacle.seat.name())
                    .expect("failed to assign seat to libinput");
                let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

                // Bind all our objects that get driven by the event loop

                let insert_ret =
                    pinnacle
                        .loop_handle
                        .insert_source(libinput_backend, move |event, _, state| {
                            state.pinnacle.apply_libinput_settings(&event);
                            state.process_input_event(event);
                        });

                if let Err(err) = insert_ret {
                    anyhow::bail!("Failed to insert libinput_backend into event loop: {err}");
                }

                pinnacle
                    .loop_handle
                    .insert_source(notifier, move |event, _, state| {
                        match event {
                            session::Event::PauseSession => {
                                let udev = state.backend.udev_mut();
                                libinput_context.suspend();
                                info!("pausing session");

                                for backend in udev.backends.values_mut() {
                                    backend.drm.pause();
                                }
                            }
                            session::Event::ActivateSession => {
                                info!("resuming session");

                                if libinput_context.resume().is_err() {
                                    error!("Failed to resume libinput context");
                                }

                                let udev = state.backend.udev_mut();
                                let pinnacle = &mut state.pinnacle;

                                let (mut device_list, connected_devices, disconnected_devices) = {
                                    let device_list = udev
                                        .udev_dispatcher
                                        .as_source_ref()
                                        .device_list()
                                        .flat_map(|(id, path)| {
                                            Some((
                                                DrmNode::from_dev_id(id).ok()?,
                                                path.to_path_buf(),
                                            ))
                                        })
                                        .collect::<HashMap<_, _>>();

                                    let (connected_devices, disconnected_devices) =
                                        udev.backends.keys().copied().partition::<Vec<_>, _>(
                                            |node| device_list.contains_key(node),
                                        );

                                    (device_list, connected_devices, disconnected_devices)
                                };

                                for node in disconnected_devices {
                                    device_list.remove(&node);
                                    udev.device_removed(pinnacle, node);
                                }

                                for node in connected_devices {
                                    device_list.remove(&node);

                                    // INFO: see if this can be moved below udev.device_changed
                                    {
                                        let Some(backend) = udev.backends.get_mut(&node) else {
                                            unreachable!();
                                        };

                                        if let Err(err) = backend.drm.activate(true) {
                                            error!("Error activating DRM device: {err}");
                                        }
                                    }

                                    udev.device_changed(pinnacle, node);

                                    let Some(backend) = udev.backends.get_mut(&node) else {
                                        unreachable!();
                                    };

                                    // Apply pending gammas
                                    //
                                    // Also welcome to some really doodoo code

                                    for (crtc, surface) in backend.surfaces.iter_mut() {
                                        match std::mem::take(&mut surface.pending_gamma_change) {
                                            PendingGammaChange::Idle => {
                                                debug!("Restoring from previous gamma");
                                                if let Err(err) = Udev::set_gamma_internal(
                                                    &backend.drm,
                                                    crtc,
                                                    surface.previous_gamma.clone(),
                                                ) {
                                                    warn!("Failed to reset gamma: {err}");
                                                    surface.previous_gamma = None;
                                                }
                                            }
                                            PendingGammaChange::Restore => {
                                                debug!("Restoring to original gamma");
                                                if let Err(err) = Udev::set_gamma_internal(
                                                    &backend.drm,
                                                    crtc,
                                                    None::<[&[u16]; 3]>,
                                                ) {
                                                    warn!("Failed to reset gamma: {err}");
                                                }
                                                surface.previous_gamma = None;
                                            }
                                            PendingGammaChange::Change(gamma) => {
                                                debug!("Changing to pending gamma");
                                                match Udev::set_gamma_internal(
                                                    &backend.drm,
                                                    crtc,
                                                    Some([&gamma[0], &gamma[1], &gamma[2]]),
                                                ) {
                                                    Ok(()) => {
                                                        surface.previous_gamma = Some(gamma);
                                                    }
                                                    Err(err) => {
                                                        warn!("Failed to set pending gamma: {err}");
                                                        surface.previous_gamma = None;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                // Newly connected devices
                                for (node, path) in device_list.into_iter() {
                                    if let Err(err) = state.backend.udev_mut().device_added(
                                        &mut state.pinnacle,
                                        node,
                                        &path,
                                    ) {
                                        error!("Error adding device: {err}");
                                    }
                                }

                                for output in
                                    state.pinnacle.space.outputs().cloned().collect::<Vec<_>>()
                                {
                                    state.schedule_render(&output);
                                }
                            }
                        }
                    })
                    .expect("failed to insert libinput notifier into event loop");

                pinnacle.shm_state.update_formats(
                    udev.gpu_manager
                        .single_renderer(&primary_gpu)?
                        .shm_formats(),
                );

                let mut renderer = udev.gpu_manager.single_renderer(&primary_gpu)?;

                info!(
                    ?primary_gpu,
                    "Trying to initialize EGL Hardware Acceleration",
                );

                match renderer.bind_wl_display(&udev.display_handle) {
                    Ok(_) => info!("EGL hardware-acceleration enabled"),
                    Err(err) => error!(?err, "Failed to initialize EGL hardware-acceleration"),
                }

                // init dmabuf support with format list from our primary gpu
                let dmabuf_formats = renderer.dmabuf_formats();
                let default_feedback =
                    DmabufFeedbackBuilder::new(primary_gpu.dev_id(), dmabuf_formats)
                        .build()
                        .expect("failed to create dmabuf feedback");
                let mut dmabuf_state = DmabufState::new();
                let global = dmabuf_state.create_global_with_default_feedback::<State>(
                    &udev.display_handle,
                    &default_feedback,
                );
                udev.dmabuf_state = Some((dmabuf_state, global));

                let gpu_manager = &mut udev.gpu_manager;
                udev.backends.values_mut().for_each(|backend_data| {
                    // Update the per drm surface dmabuf feedback
                    backend_data.surfaces.values_mut().for_each(|surface_data| {
                        surface_data.dmabuf_feedback =
                            surface_data.dmabuf_feedback.take().or_else(|| {
                                get_surface_dmabuf_feedback(
                                    primary_gpu,
                                    surface_data.render_node,
                                    gpu_manager,
                                    &surface_data.compositor,
                                )
                            });
                    });
                });

                Ok(udev)
            }),
        })
    }

    /// Schedule a new render that will cause the compositor to redraw everything.
    pub fn schedule_render(&mut self, loop_handle: &LoopHandle<State>, output: &Output) {
        let Some(surface) = render_surface_for_output(output, &mut self.backends) else {
            debug!("no render surface on output {}", output.name());
            return;
        };

        // tracing::info!(state = ?surface.render_state, name = output.name());

        match &surface.render_state {
            RenderState::Idle => {
                let output = output.clone();
                let token = loop_handle.insert_idle(move |state| {
                    state
                        .backend
                        .udev_mut()
                        .render_surface(&mut state.pinnacle, &output);
                });

                surface.render_state = RenderState::Scheduled(token);
            }
            RenderState::Scheduled(_) => (),
            RenderState::WaitingForVblank { dirty: _ } => {
                surface.render_state = RenderState::WaitingForVblank { dirty: true }
            }
        }
    }

    pub(super) fn set_output_powered(&mut self, output: &Output, powered: bool) {
        let UdevOutputData { device_id, crtc } =
            output.user_data().get::<UdevOutputData>().unwrap();

        let Some(backend_data) = self.backends.get_mut(device_id) else {
            return;
        };

        if powered {
            output.with_state_mut(|state| state.powered = true);
        } else {
            set_crtc_active(&backend_data.drm, *crtc, false);

            output.with_state_mut(|state| state.powered = false);

            // Resetting the compositor state will cause a queue frame to change the
            // crtc state to active
            //
            // Used to enable a CRTC within an atomic operation
            if let Err(err) = backend_data
                .surfaces
                .get_mut(crtc)
                .unwrap()
                .compositor
                .reset_state()
            {
                warn!("Failed to reset compositor state on crtc {crtc:?}: {err}");
            }

            if let Some(surface) = render_surface_for_output(output, &mut self.backends) {
                if let RenderState::Scheduled(idle) = std::mem::take(&mut surface.render_state) {
                    idle.cancel();
                }
            }
        }
    }
}

impl State {
    /// Switch the tty.
    ///
    /// Does nothing when called on the winit backend.
    pub fn switch_vt(&mut self, vt: i32) {
        if let Backend::Udev(udev) = &mut self.backend {
            if let Err(err) = udev.session.change_vt(vt) {
                error!("Failed to switch to vt {vt}: {err}");
            }
        }
    }
}

impl BackendData for Udev {
    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn reset_buffers(&mut self, output: &Output) {
        if let Some(id) = output.user_data().get::<UdevOutputData>() {
            if let Some(gpu) = self.backends.get_mut(&id.device_id) {
                if let Some(surface) = gpu.surfaces.get_mut(&id.crtc) {
                    surface.compositor.reset_buffers();
                }
            }
        }
    }

    fn early_import(&mut self, surface: &WlSurface) {
        if let Err(err) = self.gpu_manager.early_import(self.primary_gpu, surface) {
            warn!("early buffer import failed: {}", err);
        }
    }

    fn set_output_mode(&mut self, output: &Output, mode: OutputMode) {
        let drm_mode = self
            .backends
            .iter()
            .find_map(|(_, backend)| {
                backend
                    .drm_scanner
                    .crtcs()
                    .find(|(_, handle)| {
                        output
                            .user_data()
                            .get::<UdevOutputData>()
                            .is_some_and(|data| &data.crtc == handle)
                    })
                    .and_then(|(info, _)| {
                        info.modes()
                            .iter()
                            .find(|m| smithay::output::Mode::from(**m) == mode.into())
                    })
                    .copied()
            })
            .unwrap_or_else(|| {
                info!("Unknown mode for {}, creating new one", output.name());
                match mode {
                    OutputMode::Smithay(mode) => {
                        create_drm_mode(mode.size.w, mode.size.h, Some(mode.refresh as u32))
                    }
                    OutputMode::Drm(mode) => mode,
                }
            });

        if let Some(render_surface) = render_surface_for_output(output, &mut self.backends) {
            match render_surface.compositor.use_mode(drm_mode) {
                Ok(()) => {
                    let mode = smithay::output::Mode::from(mode);
                    info!(
                        "Set {}'s mode to {}x{}@{:.3}Hz",
                        output.name(),
                        mode.size.w,
                        mode.size.h,
                        mode.refresh as f64 / 1000.0
                    );
                    output.change_current_state(Some(mode), None, None, None);
                    output.with_state_mut(|state| {
                        // TODO: push or no?
                        if !state.modes.contains(&mode) {
                            state.modes.push(mode);
                        }
                    });
                }
                Err(err) => warn!("Failed to set output mode for {}: {err}", output.name()),
            }
        }
    }
}

// TODO: document desperately
struct UdevBackendData {
    surfaces: HashMap<crtc::Handle, RenderSurface>,
    gbm: GbmDevice<DrmDeviceFd>,
    drm: DrmDevice,
    drm_scanner: DrmScanner,
    render_node: DrmNode,
    registration_token: RegistrationToken,
}

#[derive(Debug, thiserror::Error)]
enum DeviceAddError {
    #[error("Failed to open device using libseat: {0}")]
    DeviceOpen(libseat::Error),
    #[error("Failed to initialize drm device: {0}")]
    DrmDevice(DrmError),
    #[error("Failed to initialize gbm device: {0}")]
    GbmDevice(std::io::Error),
    #[error("Failed to access drm node: {0}")]
    DrmNode(CreateDrmNodeError),
    #[error("Failed to add device to GpuManager: {0}")]
    AddNode(egl::Error),
}

fn get_surface_dmabuf_feedback(
    primary_gpu: DrmNode,
    render_node: DrmNode,
    gpu_manager: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    composition: &GbmDrmCompositor,
) -> Option<DrmSurfaceDmabufFeedback> {
    let primary_formats = gpu_manager
        .single_renderer(&primary_gpu)
        .ok()?
        .dmabuf_formats();

    let render_formats = gpu_manager
        .single_renderer(&render_node)
        .ok()?
        .dmabuf_formats();

    let all_render_formats = primary_formats
        .iter()
        .chain(render_formats.iter())
        .copied()
        .collect::<IndexSet<_>>();

    let surface = composition.surface();
    let planes = surface.planes().clone();
    // We limit the scan-out trache to formats we can also render from
    // so that there is always a fallback render path available in case
    // the supplied buffer can not be scanned out directly
    let planes_formats = planes
        .primary
        .into_iter()
        .flat_map(|p| p.formats)
        .chain(planes.overlay.into_iter().flat_map(|p| p.formats))
        .collect::<IndexSet<_>>()
        .intersection(&all_render_formats)
        .copied()
        .collect::<Vec<_>>();

    let builder = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), primary_formats);
    let render_feedback = builder
        .clone()
        .add_preference_tranche(render_node.dev_id(), None, render_formats.clone())
        .build()
        .ok()?; // INFO: this is an unwrap in Anvil, does it matter?

    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().ok()?, // INFO: this is an unwrap in Anvil, does it matter?
            Some(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout),
            planes_formats,
        )
        .add_preference_tranche(render_node.dev_id(), None, render_formats)
        .build()
        .ok()?; // INFO: this is an unwrap in Anvil, does it matter?

    Some(DrmSurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}

struct DrmSurfaceDmabufFeedback {
    render_feedback: DmabufFeedback,
    scanout_feedback: DmabufFeedback,
}

/// The state of a [`RenderSurface`].
#[derive(Debug, Default)]
enum RenderState {
    /// No render is scheduled.
    #[default]
    Idle,
    // TODO: remove the token on tty switch or output unplug
    /// A render has been queued.
    Scheduled(
        /// The idle token from a render being scheduled.
        /// This is used to cancel renders if, for example,
        /// the output being rendered is removed.
        Idle<'static>,
    ),
    /// A frame was rendered and scheduled and we are waiting for vblank.
    WaitingForVblank {
        /// A render was scheduled while waiting for vblank.
        /// In this case, another render will be scheduled once vblank happens.
        dirty: bool,
    },
}

/// Render surface for an output.
struct RenderSurface {
    /// The node from `connector_connected`.
    device_id: DrmNode,
    /// The node rendering to the screen? idk
    ///
    /// If this is equal to the primary gpu node then it does the rendering operations.
    /// If it's not it is the node the composited buffer ends up on.
    render_node: DrmNode,
    /// The thing rendering elements and queueing frames.
    compositor: GbmDrmCompositor,
    dmabuf_feedback: Option<DrmSurfaceDmabufFeedback>,
    render_state: RenderState,
    screencopy_commit_state: ScreencopyCommitState,

    previous_gamma: Option<[Box<[u16]>; 3]>,
    pending_gamma_change: PendingGammaChange,
}

#[derive(Debug, Clone, Default)]
enum PendingGammaChange {
    /// No pending gamma
    #[default]
    Idle,
    /// Restore the original gamma
    Restore,
    /// Change the gamma
    Change([Box<[u16]>; 3]),
}

#[derive(Default, Debug, Clone, Copy)]
struct ScreencopyCommitState {
    primary_plane_swapchain: CommitCounter,
    primary_plane_element: CommitCounter,
    cursor: CommitCounter,
}

type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmDevice<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

/// Render a frame with the given elements.
///
/// This frame needs to be queued for scanout afterwards.
fn render_frame<'a>(
    compositor: &mut GbmDrmCompositor,
    renderer: &mut UdevRenderer<'a>,
    elements: &'a [OutputRenderElement<UdevRenderer<'a>>],
    clear_color: [f32; 4],
) -> Result<UdevRenderFrameResult<'a>, SwapBuffersError> {
    use smithay::backend::drm::compositor::RenderFrameError;

    compositor
        .render_frame(renderer, elements, clear_color)
        .map_err(|err| match err {
            RenderFrameError::PrepareFrame(err) => err.into(),
            RenderFrameError::RenderFrame(damage::Error::Rendering(err)) => err.into(),
            _ => unreachable!(),
        })
}

impl Udev {
    pub fn renderer(&mut self) -> anyhow::Result<UdevRenderer<'_>> {
        Ok(self.gpu_manager.single_renderer(&self.primary_gpu)?)
    }

    /// A GPU was plugged in.
    fn device_added(
        &mut self,
        pinnacle: &mut Pinnacle,
        node: DrmNode,
        path: &Path,
    ) -> Result<(), DeviceAddError> {
        // Try to open the device
        let fd = self
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(DeviceAddError::DeviceOpen)?;

        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) =
            DrmDevice::new(fd.clone(), true).map_err(DeviceAddError::DrmDevice)?;
        let gbm = GbmDevice::new(fd).map_err(DeviceAddError::GbmDevice)?;

        let registration_token = pinnacle
            .loop_handle
            .insert_source(notifier, move |event, metadata, state| match event {
                DrmEvent::VBlank(crtc) => {
                    state
                        .backend
                        .udev_mut()
                        .on_vblank(&mut state.pinnacle, node, crtc, metadata);
                }
                DrmEvent::Error(error) => {
                    error!("{:?}", error);
                }
            })
            .expect("failed to insert drm notifier into event loop");

        // SAFETY: no clue lol just copied this from anvil
        let render_node = EGLDevice::device_for_display(&unsafe {
            EGLDisplay::new(gbm.clone()).expect("failed to create EGLDisplay")
        })
        .ok()
        .and_then(|x| x.try_get_render_node().ok().flatten())
        .unwrap_or(node);

        self.gpu_manager
            .as_mut()
            .add_node(render_node, gbm.clone())
            .map_err(DeviceAddError::AddNode)?;

        self.backends.insert(
            node,
            UdevBackendData {
                registration_token,
                gbm,
                drm,
                drm_scanner: DrmScanner::new(),
                render_node,
                surfaces: HashMap::new(),
            },
        );

        self.device_changed(pinnacle, node);

        Ok(())
    }

    /// A display was plugged in.
    fn connector_connected(
        &mut self,
        pinnacle: &mut Pinnacle,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let device = if let Some(device) = self.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let mut renderer = self
            .gpu_manager
            .single_renderer(&device.render_node)
            .expect("failed to get primary gpu MultiRenderer");
        let render_formats = renderer
            .as_mut()
            .egl_context()
            .dmabuf_render_formats()
            .clone();

        info!(
            ?crtc,
            "Trying to setup connector {:?}-{}",
            connector.interface(),
            connector.interface_id(),
        );

        let mode_id = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);

        let drm_mode = connector.modes()[mode_id];
        let wl_mode = smithay::output::Mode::from(drm_mode);

        let surface = match device
            .drm
            .create_surface(crtc, drm_mode, &[connector.handle()])
        {
            Ok(surface) => surface,
            Err(err) => {
                warn!("Failed to create drm surface: {}", err);
                return;
            }
        };

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );

        let (make, model, serial) = EdidInfo::try_from_connector(&device.drm, connector.handle())
            .map(|info| (info.manufacturer, info.model, info.serial))
            .unwrap_or_else(|err| {
                warn!("Failed to parse EDID info: {err}");
                ("Unknown".into(), "Unknown".into(), None)
            });

        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));

        if pinnacle.outputs.keys().any(|op| {
            op.user_data()
                .get::<UdevOutputData>()
                .is_some_and(|op_id| op_id.crtc == crtc)
        }) {
            return;
        }

        let output = Output::new(
            output_name,
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: Subpixel::from(connector.subpixel()),
                make,
                model,
            },
        );
        let global = output.create_global::<State>(&self.display_handle);

        pinnacle.outputs.insert(output.clone(), Some(global));

        output.with_state_mut(|state| {
            state.serial = serial;
        });

        output.set_preferred(wl_mode);

        let modes = connector
            .modes()
            .iter()
            .cloned()
            .map(smithay::output::Mode::from)
            .collect::<Vec<_>>();
        output.with_state_mut(|state| state.modes = modes);

        pinnacle
            .output_management_manager_state
            .add_head::<State>(&output);

        let x = pinnacle.space.outputs().fold(0, |acc, o| {
            let Some(geo) = pinnacle.space.output_geometry(o) else {
                unreachable!()
            };
            acc + geo.size.w
        });
        let position = (x, 0).into();

        output.user_data().insert_if_missing(|| UdevOutputData {
            crtc,
            device_id: node,
        });

        let allocator = GbmAllocator::new(
            device.gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );

        // I like how this is still in here
        let color_formats = if std::env::var("ANVIL_DISABLE_10BIT").is_ok() {
            SUPPORTED_FORMATS_8BIT_ONLY
        } else {
            SUPPORTED_FORMATS
        };

        let compositor = {
            let mut planes = surface.planes().clone();

            // INFO: We are disabling overlay planes because it seems that any elements on
            // overlay planes don't get up/downscaled according to the set filter;
            // it always defaults to linear. Also alacritty seems to have the wrong alpha
            // when it's on the overlay plane.
            planes.overlay.clear();

            match DrmCompositor::new(
                &output,
                surface,
                Some(planes),
                allocator,
                device.gbm.clone(),
                color_formats,
                render_formats,
                device.drm.cursor_size(),
                Some(device.gbm.clone()),
            ) {
                Ok(compositor) => compositor,
                Err(err) => {
                    warn!("Failed to create drm compositor: {}", err);
                    return;
                }
            }
        };

        let dmabuf_feedback = get_surface_dmabuf_feedback(
            self.primary_gpu,
            device.render_node,
            &mut self.gpu_manager,
            &compositor,
        );

        let surface = RenderSurface {
            device_id: node,
            render_node: device.render_node,
            compositor,
            dmabuf_feedback,
            render_state: RenderState::Idle,
            screencopy_commit_state: ScreencopyCommitState::default(),
            previous_gamma: None,
            pending_gamma_change: PendingGammaChange::Idle,
        };

        device.surfaces.insert(crtc, surface);

        pinnacle.change_output_state(
            self,
            &output,
            Some(OutputMode::Smithay(wl_mode)),
            None,
            None,
            Some(position),
        );

        // If there is saved connector state, the connector was previously plugged in.
        // In this case, restore its tags and location.
        // TODO: instead of checking the connector, check the monitor's edid info instead
        if let Some(saved_state) = pinnacle
            .config
            .connector_saved_states
            .get(&OutputName(output.name()))
        {
            let ConnectorSavedState { loc, tags, scale } = saved_state;
            output.with_state_mut(|state| state.tags.clone_from(tags));
            pinnacle.change_output_state(self, &output, None, None, *scale, Some(*loc));
        } else {
            pinnacle.signal_state.output_connect.signal(|buffer| {
                buffer.push_back(OutputConnectResponse {
                    output_name: Some(output.name()),
                })
            });
        }

        pinnacle.output_management_manager_state.update::<State>();
    }

    /// A display was unplugged.
    fn connector_disconnected(
        &mut self,
        pinnacle: &mut Pinnacle,
        node: DrmNode,
        crtc: crtc::Handle,
    ) {
        debug!(?crtc, "connector_disconnected");

        let device = if let Some(device) = self.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        device.surfaces.remove(&crtc);

        let output = pinnacle
            .outputs
            .keys()
            .find(|o| {
                o.user_data()
                    .get::<UdevOutputData>()
                    .map(|id| id.device_id == node && id.crtc == crtc)
                    .unwrap_or(false)
            })
            .cloned();

        if let Some(output) = output {
            pinnacle.remove_output(&output);
        }
    }

    fn device_changed(&mut self, pinnacle: &mut Pinnacle, node: DrmNode) {
        let device = if let Some(device) = self.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let drm_scan_result = match device.drm_scanner.scan_connectors(&device.drm) {
            Ok(event) => event,
            Err(err) => {
                error!("Failed to scan drm connectors: {err}");
                return;
            }
        };

        for event in drm_scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_connected(pinnacle, node, connector, crtc);
                }
                DrmScanEvent::Disconnected {
                    connector: _,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(pinnacle, node, crtc);
                }
                _ => {}
            }
        }
    }

    /// A GPU was unplugged.
    fn device_removed(&mut self, pinnacle: &mut Pinnacle, node: DrmNode) {
        let Some(device) = self.backends.get(&node) else {
            return;
        };

        let crtcs = device
            .drm_scanner
            .crtcs()
            .map(|(info, crtc)| (info.clone(), crtc))
            .collect::<Vec<_>>();

        for (_connector, crtc) in crtcs {
            self.connector_disconnected(pinnacle, node, crtc);
        }

        debug!("Surfaces dropped");

        // drop the backends on this side
        if let Some(backend_data) = self.backends.remove(&node) {
            self.gpu_manager
                .as_mut()
                .remove_node(&backend_data.render_node);

            pinnacle.loop_handle.remove(backend_data.registration_token);

            debug!("Dropping device");
        }
    }

    /// Mark [`OutputPresentationFeedback`]s as presented and schedule a new render on idle.
    fn on_vblank(
        &mut self,
        pinnacle: &mut Pinnacle,
        dev_id: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        let Some(surface) = self
            .backends
            .get_mut(&dev_id)
            .and_then(|device| device.surfaces.get_mut(&crtc))
        else {
            return;
        };

        let output = if let Some(output) = pinnacle.outputs.keys().find(|o| {
            let udev_op_data = o.user_data().get::<UdevOutputData>();
            udev_op_data
                .is_some_and(|data| data.device_id == surface.device_id && data.crtc == crtc)
        }) {
            output.clone()
        } else {
            // somehow we got called with an invalid output
            return;
        };

        match surface
            .compositor
            .frame_submitted()
            .map_err(SwapBuffersError::from)
        {
            Ok(user_data) => {
                if let Some(mut feedback) = user_data.flatten() {
                    let tp = metadata.as_ref().and_then(|metadata| match metadata.time {
                        smithay::backend::drm::DrmEventTime::Monotonic(tp) => Some(tp),
                        smithay::backend::drm::DrmEventTime::Realtime(_) => None,
                    });
                    let seq = metadata
                        .as_ref()
                        .map(|metadata| metadata.sequence)
                        .unwrap_or(0);

                    let (clock, flags) = if let Some(tp) = tp {
                        (
                            tp.into(),
                            wp_presentation_feedback::Kind::Vsync
                                | wp_presentation_feedback::Kind::HwClock
                                | wp_presentation_feedback::Kind::HwCompletion,
                        )
                    } else {
                        (pinnacle.clock.now(), wp_presentation_feedback::Kind::Vsync)
                    };

                    feedback.presented(
                        clock,
                        output
                            .current_mode()
                            .map(|mode| Duration::from_secs_f64(1000f64 / mode.refresh as f64))
                            .unwrap_or_default(),
                        seq as u64,
                        flags,
                    );
                }

                output.with_state_mut(|state| {
                    if let BlankingState::Blanking = state.blanking_state {
                        debug!("Output {} blanked", output.name());
                        state.blanking_state = BlankingState::Blanked;
                    }
                })
            }
            Err(err) => {
                warn!("Error during rendering: {:?}", err);
                if let SwapBuffersError::ContextLost(err) = err {
                    panic!("Rendering loop lost: {}", err)
                }
            }
        };

        let dirty = match std::mem::take(&mut surface.render_state) {
            RenderState::WaitingForVblank { dirty } => dirty,
            state => {
                debug!("vblank happened but render state was {state:?}",);
                self.schedule_render(&pinnacle.loop_handle, &output);
                return;
            }
        };

        if dirty {
            self.schedule_render(&pinnacle.loop_handle, &output);
        } else {
            for window in pinnacle.windows.iter() {
                window.send_frame(
                    &output,
                    pinnacle.clock.now(),
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }

        // Schedule a render when the next frame of an animated cursor should be drawn.
        //
        // TODO: Remove this and improve the render pipeline.
        // Because of how the event loop works and the current implementation of rendering,
        // immediately queuing a render here has the possibility of not submitting a new frame to
        // DRM, meaning no vblank. The event loop will wait as it has no events, so things like
        // animated cursors may hitch and only update when, for example, the cursor is actively
        // moving as this generates events.
        //
        // What we should do is what Niri does: if `render_surface` doesn't cause any damage,
        // instead of setting the `RenderState` to Idle, set it to some "waiting for estimated
        // vblank" state and have `render_surface` always schedule a timer to fire at the estimated
        // vblank time that will attempt another render schedule.
        //
        // This has the advantage of scheduling a render in a source and not in an idle callback,
        // meaning we are guarenteed to have a render happen immediately and we won't have to wait
        // for another event or call `loop_signal.wakeup()`.
        if let Some(until) = pinnacle
            .cursor_state
            .time_until_next_animated_cursor_frame()
        {
            let _ = pinnacle.loop_handle.insert_source(
                Timer::from_duration(until),
                move |_, _, state| {
                    state.schedule_render(&output);
                    calloop::timer::TimeoutAction::Drop
                },
            );
        }
    }

    /// Render to the [`RenderSurface`] associated with the given `output`.
    #[tracing::instrument(level = "debug", skip(self, pinnacle), fields(output = output.name()))]
    fn render_surface(&mut self, pinnacle: &mut Pinnacle, output: &Output) {
        let Some(surface) = render_surface_for_output(output, &mut self.backends) else {
            return;
        };

        if !pinnacle.outputs.contains_key(output) {
            surface.render_state = RenderState::Idle;
            return;
        }

        // TODO: possibly lift this out and make it so that scheduling a render
        // does nothing on powered off outputs
        if output.with_state(|state| !state.powered) {
            surface.render_state = RenderState::Idle;
            return;
        }

        assert_matches!(surface.render_state, RenderState::Scheduled(_));

        let render_node = surface.render_node;
        let primary_gpu = self.primary_gpu;
        let mut renderer = if primary_gpu == render_node {
            self.gpu_manager.single_renderer(&render_node)
        } else {
            let format = surface.compositor.format();
            self.gpu_manager
                .renderer(&primary_gpu, &render_node, format)
        }
        .expect("failed to create MultiRenderer");

        let _ = renderer.upscale_filter(self.upscale_filter);
        let _ = renderer.downscale_filter(self.downscale_filter);

        ///////////////////////////////////////////////////////////////////////////////////////////

        // draw the cursor as relevant and
        // reset the cursor if the surface is no longer alive
        if let CursorImageStatus::Surface(surface) = &pinnacle.cursor_state.cursor_image() {
            if !surface.alive() {
                pinnacle
                    .cursor_state
                    .set_cursor_image(CursorImageStatus::default_named());
            }
        }

        ///////////////////////////////////////////////////////////////////////////////////////////

        let pointer_location = pinnacle
            .seat
            .get_pointer()
            .map(|ptr| ptr.current_location())
            .unwrap_or((0.0, 0.0).into());

        let mut output_render_elements = Vec::new();

        let should_blank = pinnacle.lock_state.is_locking()
            || (pinnacle.lock_state.is_locked()
                && output.with_state(|state| state.lock_surface.is_none()));

        let (pointer_render_elements, cursor_ids) = pointer_render_elements(
            output,
            &mut renderer,
            &mut pinnacle.cursor_state,
            &pinnacle.space,
            pointer_location,
            pinnacle.dnd_icon.as_ref(),
            &pinnacle.clock,
        );
        output_render_elements.extend(
            pointer_render_elements
                .into_iter()
                .map(OutputRenderElement::from),
        );

        if should_blank {
            output.with_state_mut(|state| {
                if let BlankingState::NotBlanked = state.blanking_state {
                    debug!("Blanking output {} for session lock", output.name());
                    state.blanking_state = BlankingState::Blanking;
                }
            });
        } else if pinnacle.lock_state.is_locked() {
            if let Some(lock_surface) = output.with_state(|state| state.lock_surface.clone()) {
                let elems = render_elements_from_surface_tree(
                    &mut renderer,
                    lock_surface.wl_surface(),
                    (0, 0),
                    output.current_scale().fractional_scale(),
                    1.0,
                    element::Kind::Unspecified,
                );

                output_render_elements.extend(elems);
            }
        } else {
            let windows = pinnacle.space.elements().cloned().collect::<Vec<_>>();

            output_render_elements.extend(crate::render::output_render_elements(
                output,
                &mut renderer,
                &pinnacle.space,
                &windows,
            ));
        }

        // HACK: Taking the transaction before creating render elements
        // leads to a possibility where the original buffer still gets displayed.
        // Need to figure that out.
        // In the meantime we take the transaction afterwards and schedule another render.
        let mut render_after_transaction_finish = false;
        output.with_state_mut(|state| {
            if state
                .layout_transaction
                .as_ref()
                .is_some_and(|ts| ts.ready())
            {
                state.layout_transaction.take();
                render_after_transaction_finish = true;
            }
        });

        let clear_color = if pinnacle.lock_state.is_unlocked() {
            CLEAR_COLOR
        } else {
            CLEAR_COLOR_LOCKED
        };

        let result = (|| -> Result<bool, SwapBuffersError> {
            let render_frame_result = render_frame(
                &mut surface.compositor,
                &mut renderer,
                &output_render_elements,
                clear_color,
            )?;

            if let PrimaryPlaneElement::Swapchain(element) = &render_frame_result.primary_element {
                if let Err(err) = element.sync.wait() {
                    warn!("Failed to wait for sync point: {err}");
                }
            }

            if pinnacle.lock_state.is_unlocked() {
                handle_pending_screencopy(
                    &mut renderer,
                    output,
                    surface,
                    &render_frame_result,
                    &pinnacle.loop_handle,
                    cursor_ids,
                );
            }

            super::post_repaint(
                output,
                &render_frame_result.states,
                &pinnacle.space,
                surface
                    .dmabuf_feedback
                    .as_ref()
                    .map(|feedback| SurfaceDmabufFeedback {
                        render_feedback: &feedback.render_feedback,
                        scanout_feedback: &feedback.scanout_feedback,
                    }),
                Duration::from(pinnacle.clock.now()),
                pinnacle.cursor_state.cursor_image(),
            );

            let rendered = !render_frame_result.is_empty;

            if rendered {
                let output_presentation_feedback = take_presentation_feedback(
                    output,
                    &pinnacle.space,
                    &render_frame_result.states,
                );

                surface
                    .compositor
                    .queue_frame(Some(output_presentation_feedback))
                    .map_err(SwapBuffersError::from)?;
            }

            Ok(rendered)
        })();

        match result {
            Ok(true) => surface.render_state = RenderState::WaitingForVblank { dirty: false },
            // TODO: Don't immediately set this to Idle; this allows hot loops of `render_surface`.
            // Instead, pull a Niri and schedule a timer for the next estimated vblank to allow
            // another scheduled render.
            Ok(false) | Err(_) => surface.render_state = RenderState::Idle,
        }

        if render_after_transaction_finish {
            self.schedule_render(&pinnacle.loop_handle, output);
        }
    }
}

fn render_surface_for_output<'a>(
    output: &Output,
    backends: &'a mut HashMap<DrmNode, UdevBackendData>,
) -> Option<&'a mut RenderSurface> {
    let UdevOutputData { device_id, crtc } = output.user_data().get()?;

    backends
        .get_mut(device_id)
        .and_then(|device| device.surfaces.get_mut(crtc))
}

fn handle_pending_screencopy<'a>(
    renderer: &mut UdevRenderer<'a>,
    output: &Output,
    surface: &mut RenderSurface,
    render_frame_result: &UdevRenderFrameResult<'a>,
    loop_handle: &LoopHandle<'static, State>,
    cursor_ids: Vec<Id>,
) {
    let Some(mut screencopy) = output.with_state_mut(|state| state.screencopy.take()) else {
        return;
    };
    assert!(screencopy.output() == output);

    let untransformed_output_size = output.current_mode().expect("output no mode").size;

    let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());

    if screencopy.with_damage() {
        if render_frame_result.is_empty {
            output.with_state_mut(|state| state.screencopy.replace(screencopy));
            return;
        }

        // Compute damage
        //
        // I have no idea if the damage event is supposed to send rects local to the output or to the
        // region. Sway does the former, Hyprland the latter. Also, no one actually seems to be using the
        // received damage. wf-recorder and wl-mirror have no-op handlers for the damage event.

        let mut damage = match &render_frame_result.primary_element {
            PrimaryPlaneElement::Swapchain(element) => {
                let swapchain_commit = &mut surface.screencopy_commit_state.primary_plane_swapchain;
                let damage = element.damage.damage_since(Some(*swapchain_commit));
                *swapchain_commit = element.damage.current_commit();
                damage.map(|dmg| {
                    dmg.into_iter()
                        .map(|rect| {
                            rect.to_logical(1, Transform::Normal, &rect.size)
                                .to_physical(1)
                        })
                        .collect()
                })
            }
            PrimaryPlaneElement::Element(element) => {
                // INFO: Is this element guaranteed to be the same size as the
                // |     output? If not this becomes a
                // FIXME: offset the damage by the element's location
                //
                // also is this even ever reachable?
                let element_commit = &mut surface.screencopy_commit_state.primary_plane_element;
                let damage = element.damage_since(scale, Some(*element_commit));
                *element_commit = element.current_commit();
                Some(damage)
            }
        }
        .unwrap_or_else(|| {
            // Returning `None` means the previous CommitCounter is too old or damage
            // was reset, so damage the whole output
            DamageSet::from_slice(&[Rectangle::from_loc_and_size(
                Point::from((0, 0)),
                untransformed_output_size,
            )])
        });

        let cursor_damage = render_frame_result
            .cursor_element
            .map(|cursor| {
                let damage =
                    cursor.damage_since(scale, Some(surface.screencopy_commit_state.cursor));
                surface.screencopy_commit_state.cursor = cursor.current_commit();
                damage
            })
            .unwrap_or_default();

        damage = damage.into_iter().chain(cursor_damage).collect();

        // The primary plane and cursor had no damage but something got rendered,
        // so it must be the cursor moving.
        //
        // We currently have overlay planes disabled, so we don't have to worry about that.
        if damage.is_empty() && !render_frame_result.is_empty {
            if let Some(cursor_elem) = render_frame_result.cursor_element {
                damage = damage
                    .into_iter()
                    .chain([cursor_elem.geometry(scale)])
                    .collect();
            }
        }

        // INFO: Protocol states that `copy_with_damage` should wait until there is
        // |     damage to be copied.
        // |.
        // |     Now, for region screencopies this currently submits the frame if there is
        // |     *any* damage on the output, not just in the region. I've found that
        // |     wf-recorder blocks until the last frame is submitted, and if I don't
        // |     send a submission because its region isn't damaged it will hang.
        // |     I'm fairly certain Sway is doing a similar thing.
        if damage.is_empty() {
            output.with_state_mut(|state| state.screencopy.replace(screencopy));
            return;
        }

        screencopy.damage(&damage);
    }

    let sync_point = if let Ok(dmabuf) = dmabuf::get_dmabuf(screencopy.buffer()).cloned() {
        trace!("Dmabuf screencopy");

        let format_correct =
            Some(dmabuf.format().code) == shm_format_to_fourcc(wl_shm::Format::Argb8888);
        let width_correct = dmabuf.width() == screencopy.physical_region().size.w as u32;
        let height_correct = dmabuf.height() == screencopy.physical_region().size.h as u32;

        if !(format_correct && width_correct && height_correct) {
            return;
        }

        (|| -> anyhow::Result<Option<SyncPoint>> {
            if screencopy.physical_region()
                == Rectangle::from_loc_and_size(Point::from((0, 0)), untransformed_output_size)
            {
                // Optimization to not have to do an extra blit;
                // just blit the whole output
                renderer.bind(dmabuf)?;

                Ok(Some(render_frame_result.blit_frame_result(
                    screencopy.physical_region().size,
                    Transform::Normal,
                    output.current_scale().fractional_scale(),
                    renderer,
                    [screencopy.physical_region()],
                    cursor_ids,
                )?))
            } else {
                // `RenderFrameResult::blit_frame_result` doesn't expose a way to
                // blit from a source rectangle, so blit into another buffer
                // then blit from that into the dmabuf.

                let output_buffer_size = untransformed_output_size
                    .to_logical(1)
                    .to_buffer(1, Transform::Normal);

                let offscreen: GlesRenderbuffer = renderer.create_buffer(
                    smithay::backend::allocator::Fourcc::Abgr8888,
                    output_buffer_size,
                )?;

                renderer.bind(offscreen.clone())?;

                let sync_point = render_frame_result.blit_frame_result(
                    untransformed_output_size,
                    Transform::Normal,
                    output.current_scale().fractional_scale(),
                    renderer,
                    [Rectangle::from_loc_and_size(
                        Point::from((0, 0)),
                        untransformed_output_size,
                    )],
                    if !screencopy.overlay_cursor() {
                        cursor_ids
                    } else {
                        Vec::new()
                    },
                )?;

                // ayo are we supposed to wait this here (granted it doesn't do anything
                // because it's always ready but I want to be correct here)
                //
                // renderer.wait(&sync_point)?; // no-op

                // INFO: I have literally no idea why but doing
                // a blit_to offscreen -> dmabuf leads to some weird
                // artifacting within the first few frames of a wf-recorder
                // recording, but doing it with the targets reversed
                // is completely fine???? Bruh that essentially runs the same internal
                // code and I don't understand why there's different behavior.
                // I can see in the code that `blit_to` is missing a `self.unbind()?`
                // call, but adding that back in doesn't fix anything. So strange
                renderer.bind(dmabuf)?;

                renderer.blit_from(
                    offscreen,
                    screencopy.physical_region(),
                    Rectangle::from_loc_and_size(
                        Point::from((0, 0)),
                        screencopy.physical_region().size,
                    ),
                    TextureFilter::Linear,
                )?;

                Ok(Some(sync_point))
            }
        })()
    } else if !matches!(
        renderer::buffer_type(screencopy.buffer()),
        Some(BufferType::Shm)
    ) {
        Err(anyhow!("not a shm buffer"))
    } else {
        trace!("Shm screencopy");

        let res = smithay::wayland::shm::with_buffer_contents_mut(
            &screencopy.buffer().clone(),
            |shm_ptr, shm_len, buffer_data| {
                // yoinked from Niri (thanks yall)
                ensure!(
                    // The buffer prefers pixels in little endian ...
                    buffer_data.format == wl_shm::Format::Argb8888
                        && buffer_data.stride == screencopy.physical_region().size.w * 4
                        && buffer_data.height == screencopy.physical_region().size.h
                        && shm_len as i32 == buffer_data.stride * buffer_data.height,
                    "invalid buffer format or size"
                );

                let src_buffer_rect = screencopy.physical_region().to_logical(1).to_buffer(
                    1,
                    Transform::Normal,
                    &screencopy.physical_region().size.to_logical(1),
                );

                let output_buffer_size = untransformed_output_size
                    .to_logical(1)
                    .to_buffer(1, Transform::Normal);

                let offscreen: GlesRenderbuffer = renderer.create_buffer(
                    smithay::backend::allocator::Fourcc::Abgr8888,
                    output_buffer_size,
                )?;

                renderer.bind(offscreen)?;

                // Blit the entire output to `offscreen`.
                // Only the needed region will be copied below
                let sync_point = render_frame_result.blit_frame_result(
                    untransformed_output_size,
                    Transform::Normal,
                    output.current_scale().fractional_scale(),
                    renderer,
                    [Rectangle::from_loc_and_size(
                        Point::from((0, 0)),
                        untransformed_output_size,
                    )],
                    if !screencopy.overlay_cursor() {
                        cursor_ids
                    } else {
                        Vec::new()
                    },
                )?;

                // Can someone explain to me why it feels like some things are
                // arbitrarily `Physical` or `Buffer`
                let mapping = renderer.copy_framebuffer(
                    src_buffer_rect,
                    smithay::backend::allocator::Fourcc::Argb8888,
                )?;

                let bytes = renderer.map_texture(&mapping)?;

                ensure!(bytes.len() == shm_len, "mapped buffer has wrong length");

                // SAFETY: TODO: safety docs
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), shm_ptr, shm_len);
                }

                Ok(Some(sync_point))
            },
        );

        let Ok(res) = res else {
            unreachable!(
                "buffer is guaranteed to be shm from above and should be managed by the shm global"
            );
        };

        res
    };

    match sync_point {
        Ok(Some(sync_point)) if !sync_point.is_reached() => {
            let Some(sync_fd) = sync_point.export() else {
                screencopy.submit(false);
                return;
            };
            let mut screencopy = Some(screencopy);
            let source = Generic::new(sync_fd, Interest::READ, calloop::Mode::OneShot);
            let res = loop_handle.insert_source(source, move |_, _, _| {
                let Some(screencopy) = screencopy.take() else {
                    unreachable!("This source is removed after one run");
                };
                screencopy.submit(false);
                trace!("Submitted screencopy");
                Ok(PostAction::Remove)
            });
            if res.is_err() {
                error!("Failed to schedule screencopy submission");
            }
        }
        Ok(_) => screencopy.submit(false),
        Err(err) => error!("Failed to submit screencopy: {err}"),
    }
}
