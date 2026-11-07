//! Hardware-accelerated client rendering through `zwp_linux_dmabuf_v1`.
//!
//! Without this protocol clients can only hand ltmi shared-memory buffers, so Mesa's Wayland EGL and
//! Xwayland's glamor both fall back to software rendering (llvmpipe). With it, clients render on the GPU
//! and pass the buffers to ltmi directly.

use std::{os::unix::fs::MetadataExt, path::PathBuf};

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        egl::EGLDevice,
        renderer::{ImportDma, gles::GlesRenderer},
    },
    reexports::wayland_server::DisplayHandle,
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

use crate::App;

/// Advertise the dmabuf formats `renderer` can import.
///
/// Must run before Xwayland and the app start: both look for the global only once, when they connect.
pub fn init(state: &mut DmabufState, dh: &DisplayHandle, renderer: &GlesRenderer) {
    let formats = renderer.dmabuf_formats();

    // Clients need the default feedback's main device to find the GPU that ltmi renders with.
    let feedback = render_node(renderer).and_then(|(path, dev)| {
        let feedback = DmabufFeedbackBuilder::new(dev, formats.clone())
            .build()
            .map_err(|err| format!("failed to build dmabuf feedback: {err}"))?;
        Ok((path, feedback))
    });
    match feedback {
        Ok((path, feedback)) => {
            tracing::info!(
                "linux-dmabuf feedback on {}, clients may use hardware OpenGL",
                path.display()
            );
            state.create_global_with_default_feedback::<App>(dh, &feedback);
        }
        Err(err) => {
            tracing::warn!("No render node for linux-dmabuf feedback ({err}), clients will use software OpenGL");
            state.create_global::<App>(dh, formats);
        }
    }
}

/// The render node of the GPU behind `renderer`'s EGL display, with its device number (`dev_t`).
fn render_node(renderer: &GlesRenderer) -> Result<(PathBuf, u64), String> {
    let device = EGLDevice::device_for_display(renderer.egl_context().display())
        .map_err(|err| format!("no EGL device: {err}"))?;
    let path = device
        .render_device_path()
        .map_err(|err| format!("no render node: {err}"))?;
    let dev = std::fs::metadata(&path)
        .map_err(|err| format!("{}: {err}", path.display()))?
        .rdev();
    Ok((path, dev))
}

/// Test-import the dmabufs clients created since the last call and tell them whether they can use them.
///
/// The renderer is not part of [`App`], so imports are queued in [`DmabufHandler::dmabuf_imported`] and
/// resolved here once per frame, before the client replies are flushed.
///
/// Must not run between binding the winit backend and submitting the frame: importing makes the EGL context
/// current without ltmi's window surface, and the swap then fails with `EGL_BAD_SURFACE`.
pub fn import_pending(state: &mut App, renderer: &mut GlesRenderer) {
    for (dmabuf, notifier) in state.pending_dmabufs.drain(..) {
        match renderer.import_dmabuf(&dmabuf, None) {
            Ok(_) => {
                // Fails only if the client is already gone, and then there is nobody left to tell.
                let _ = notifier.successful::<App>();
            }
            Err(err) => {
                tracing::warn!("Rejected client dmabuf: {err}");
                notifier.failed();
            }
        }
    }
}

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        self.pending_dmabufs.push((dmabuf, notifier));
    }
}
