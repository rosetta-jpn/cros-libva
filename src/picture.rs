// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::marker::PhantomData;
use std::rc::Rc;

use crate::bindings;
use crate::buffer::Buffer;
use crate::context::Context;
use crate::display::Display;
use crate::surface::Surface;
use crate::va_check;
use crate::Image;
use crate::SurfaceMemoryDescriptor;
use crate::VaError;

// Use the sealed trait pattern to make sure that new states are not created in caller code. More
// information about the sealed trait pattern can be found at
// <https://rust-lang.github.io/api-guidelines/future-proofing.html#sealed-traits-protect-against-downstream-implementations-c-sealed>
mod private {
    pub trait Sealed {}
}

/// A `Picture` will only have valid YUV data after a sequence of operations are performed in a
/// particular order. This order correspond to the following VA-API calls: `vaBeginPicture`,
/// `vaRenderPicture`, `vaEndPicture` and `vaSyncSurface`. This trait enforces this ordering by
/// implementing the Typestate pattern to constrain what operations are available in what particular
/// states.
///
/// The states for the state machine are:
///
/// * PictureNew -> PictureBegin
/// * PictureBegin -> PictureRender
/// * PictureRender ->PictureEnd
/// * PictureEnd -> PictureSync
///
/// Where the surface can be reclaimed in both `PictureNew` and `PictureSync`, as either no
/// operation took place (as in `PictureNew`), or it is guaranteed that the operation has already
/// completed (as in `PictureSync`).
///
/// More information about the Typestate pattern can be found at
/// <http://cliffle.com/blog/rust-typestate/>
pub trait PictureState: private::Sealed {}

/// Represents a `Picture` that has just been created.
pub enum PictureNew {}
impl PictureState for PictureNew {}
impl private::Sealed for PictureNew {}

/// Represents a `Picture` after `vaBeginPicture` has been called.
pub enum PictureBegin {}
impl PictureState for PictureBegin {}
impl private::Sealed for PictureBegin {}

/// Represents a `Picture` after `vaRenderPicture` has been called.
pub enum PictureRender {}
impl PictureState for PictureRender {}
impl private::Sealed for PictureRender {}

/// Represents a `Picture` after `vaEndPicture` has been called.
pub enum PictureEnd {}
impl PictureState for PictureEnd {}
impl private::Sealed for PictureEnd {}

/// Represents a `Picture` after `vaSyncSurface` has been called on the underlying surface.
pub enum PictureSync {}
impl PictureState for PictureSync {}
impl private::Sealed for PictureSync {}

/// Represents a state where one can reclaim the underlying `Surface` for this `Picture`. This is
/// true when either no decoding has been initiated or, alternatively, when the decoding operation
/// has completed for the underlying `vaSurface`
pub trait PictureReclaimableSurface: PictureState + private::Sealed {}
impl PictureReclaimableSurface for PictureNew {}
impl PictureReclaimableSurface for PictureSync {}

struct PictureInner<D: SurfaceMemoryDescriptor> {
    /// Timestamp of the picture.
    timestamp: u64,
    /// A context associated with this picture.
    context: Rc<Context>,
    /// Contains the buffers used to decode the data.
    buffers: Vec<Buffer>,
    /// Contains the actual decoded data. Note that the surface may be shared in
    /// interlaced decoding.
    surface: Rc<Surface<D>>,
}

/// A `Surface` that is being rendered into.
///
/// This struct abstracts the decoding flow using `vaBeginPicture`, `vaRenderPicture`,
/// `vaEndPicture` and `vaSyncSurface` in a type-safe way.
///
/// The surface will have valid picture data after all the stages of decoding are called.
pub struct Picture<S: PictureState, D: SurfaceMemoryDescriptor> {
    inner: Box<PictureInner<D>>,
    phantom: std::marker::PhantomData<S>,
}

impl<D: SurfaceMemoryDescriptor> Picture<PictureNew, D> {
    /// Creates a new Picture with a given `timestamp`. `surface` is the underlying surface that
    /// libva will render to.
    pub fn new(timestamp: u64, context: Rc<Context>, surface: Surface<D>) -> Self {
        Self {
            inner: Box::new(PictureInner {
                timestamp,
                context,
                buffers: Default::default(),
                surface: Rc::new(surface),
            }),

            phantom: PhantomData,
        }
    }

    /// Creates a new Picture with a given `frame_number` to identify it,
    /// reusing the Surface from `picture`. This is useful for interlaced
    /// decoding as one can render both fields to the same underlying surface.
    pub fn new_from_same_surface<S: PictureReclaimableSurface>(
        timestamp: u64,
        picture: &Picture<S, D>,
    ) -> Self {
        let context = Rc::clone(&picture.inner.context);
        Picture {
            inner: Box::new(PictureInner {
                timestamp,
                context,
                buffers: Default::default(),
                surface: Rc::clone(&picture.inner.surface),
            }),

            phantom: PhantomData,
        }
    }

    /// Add `buffer` to the picture.
    pub fn add_buffer(&mut self, buffer: Buffer) {
        self.inner.buffers.push(buffer);
    }

    /// Wrapper around `vaBeginPicture`.
    pub fn begin(self) -> Result<Picture<PictureBegin, D>, VaError> {
        // Safe because `self.inner.context` represents a valid VAContext and
        // `self.inner.surface` represents a valid VASurface.
        let res = va_check(unsafe {
            bindings::vaBeginPicture(
                self.inner.context.display().handle(),
                self.inner.context.id(),
                self.inner.surface.id(),
            )
        });

        res.map(|()| Picture {
            inner: self.inner,
            phantom: PhantomData,
        })
    }
}

impl<D: SurfaceMemoryDescriptor> Picture<PictureBegin, D> {
    /// Wrapper around `vaRenderPicture`.
    pub fn render(self) -> Result<Picture<PictureRender, D>, VaError> {
        // Safe because `self.inner.context` represents a valid `VAContext` and `self.inner.surface`
        // represents a valid `VASurface`. `buffers` point to a Rust struct and the vector length is
        // passed to the C function, so it is impossible to write past the end of the vector's
        // storage by mistake.
        va_check(unsafe {
            bindings::vaRenderPicture(
                self.inner.context.display().handle(),
                self.inner.context.id(),
                Buffer::as_id_vec(&self.inner.buffers).as_mut_ptr(),
                self.inner.buffers.len() as i32,
            )
        })
        .map(|()| Picture {
            inner: self.inner,
            phantom: PhantomData,
        })
    }
}

impl<D: SurfaceMemoryDescriptor> Picture<PictureRender, D> {
    /// Wrapper around `vaEndPicture`.
    pub fn end(self) -> Result<Picture<PictureEnd, D>, VaError> {
        // Safe because `self.inner.context` represents a valid `VAContext`.
        va_check(unsafe {
            bindings::vaEndPicture(
                self.inner.context.display().handle(),
                self.inner.context.id(),
            )
        })
        .map(|()| Picture {
            inner: self.inner,
            phantom: PhantomData,
        })
    }
}

impl<D: SurfaceMemoryDescriptor> Picture<PictureEnd, D> {
    /// Syncs the picture, ensuring that all pending operations are complete when this call returns.
    pub fn sync(self) -> Result<Picture<PictureSync, D>, (VaError, Self)> {
        let res = self.inner.surface.sync();

        match res {
            Ok(()) => Ok(Picture {
                inner: self.inner,
                phantom: PhantomData,
            }),
            Err(e) => Err((e, self)),
        }
    }
}

impl<D: SurfaceMemoryDescriptor> Picture<PictureSync, D> {
    /// Create a new derived image from this `Picture` using `vaDeriveImage`.
    ///
    /// Derived images are a direct view (i.e. without any copy) on the buffer content of the
    /// `Picture`. On the other hand, not all `Pictures` can be derived.
    pub fn derive_image(&self, display_resolution: (u32, u32)) -> Result<Image, VaError> {
        // An all-zero byte-pattern is a valid initial value for `VAImage`.
        let mut image: bindings::VAImage = Default::default();

        // Safe because `self` has a valid display handle and ID.
        va_check(unsafe {
            bindings::vaDeriveImage(self.display().handle(), self.inner.surface.id(), &mut image)
        })?;

        Image::new(self, image, true, display_resolution)
    }

    /// Create new image from the `Picture` using `vaCreateImage` and `vaGetImage`.
    ///
    /// The image will contain a copy of the `Picture` in the desired `format` and `coded_resolution`.
    pub fn create_image(
        &self,
        mut format: bindings::VAImageFormat,
        coded_resolution: (u32, u32),
        display_resolution: (u32, u32),
    ) -> Result<Image, VaError> {
        let dpy = self.display().handle();
        // An all-zero byte-pattern is a valid initial value for `VAImage`.
        let mut image: bindings::VAImage = Default::default();

        // Safe because `dpy` is a valid display handle.
        va_check(unsafe {
            bindings::vaCreateImage(
                dpy,
                &mut format,
                coded_resolution.0 as i32,
                coded_resolution.1 as i32,
                &mut image,
            )
        })?;

        // Safe because `dpy` is a valid display handle, `picture.surface` is a valid VASurface and
        // `image` is a valid `VAImage`.
        match va_check(unsafe {
            bindings::vaGetImage(
                dpy,
                self.inner.surface.id(),
                0,
                0,
                coded_resolution.0,
                coded_resolution.1,
                image.image_id,
            )
        }) {
            Ok(()) => Image::new(self, image, false, display_resolution),

            Err(e) => {
                // Safe because `image` is a valid `VAImage`.
                unsafe {
                    bindings::vaDestroyImage(dpy, image.image_id);
                }

                Err(e)
            }
        }
    }
}

impl<S: PictureState, D: SurfaceMemoryDescriptor> Picture<S, D> {
    /// Returns the timestamp of this picture.
    pub fn timestamp(&self) -> u64 {
        self.inner.timestamp
    }

    /// Returns the underlying surface. This is a convenience synonym for `as_ref`, to allow
    /// callers to understand what they are returning.
    pub fn surface(&self) -> &Surface<D> {
        self.as_ref()
    }

    /// Returns a reference to the display owning this `Picture`.
    pub(crate) fn display(&self) -> &Rc<Display> {
        self.inner.context.display()
    }
}

impl<S: PictureReclaimableSurface, D: SurfaceMemoryDescriptor> Picture<S, D> {
    /// Reclaim ownership of the Surface this picture has been created from, consuming the picture
    /// in the process. Useful if the Surface is part of a pool.
    ///
    /// This will fail and return the passed object if there are more than one reference to the
    /// underlying surface.
    pub fn take_surface(self) -> Result<Surface<D>, Self> {
        let inner = self.inner;
        match Rc::try_unwrap(inner.surface) {
            Ok(surface) => Ok(surface),
            Err(surface) => Err(Self {
                inner: Box::new(PictureInner {
                    surface,
                    context: inner.context,
                    buffers: inner.buffers,
                    timestamp: inner.timestamp,
                }),
                phantom: PhantomData,
            }),
        }
    }
}

impl<S: PictureState, D: SurfaceMemoryDescriptor> AsRef<Surface<D>> for Picture<S, D> {
    fn as_ref(&self) -> &Surface<D> {
        &self.inner.surface
    }
}
