// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

use std::borrow::Borrow;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;

use anyhow::anyhow;
use anyhow::Result;
use base::MappedRegion;
use base::MemoryMappingArena;
use cros_codecs::decoders::DynDecodedHandle;
use cros_codecs::decoders::VideoDecoder;
use cros_codecs::DecodedFormat;
use libva::Display;

use crate::virtio::video::decoder::Capability;
use crate::virtio::video::decoder::DecoderBackend;
use crate::virtio::video::decoder::DecoderEvent;
use crate::virtio::video::decoder::DecoderSession;
use crate::virtio::video::error::VideoError;
use crate::virtio::video::error::VideoResult;
use crate::virtio::video::format::Format;
use crate::virtio::video::format::FormatDesc;
use crate::virtio::video::format::FormatRange;
use crate::virtio::video::format::FrameFormat;
use crate::virtio::video::format::Level;
use crate::virtio::video::format::Profile;
use crate::virtio::video::format::Rect;
use crate::virtio::video::resource::BufferHandle;
use crate::virtio::video::resource::GuestResource;
use crate::virtio::video::resource::GuestResourceHandle;
use crate::virtio::video::utils::EventQueue;
use crate::virtio::video::utils::OutputQueue;

/// Represents a buffer we have not yet sent to the accelerator.
struct PendingJob {
    resource_id: u32,
    timestamp: u64,
    resource: GuestResourceHandle,
    offset: u32,
    bytes_used: u32,
}

/// A set of params returned when a dynamic resolution change is found in the
/// bitstream.
pub struct DrcParams {
    /// The minimum amount of buffers needed to decode the stream.
    min_num_buffers: usize,
    /// The stream's new width.
    width: u32,
    /// The stream's new height.
    height: u32,
    /// The visible resolution.
    visible_rect: Rect,
}

impl TryFrom<DecodedFormat> for Format {
    type Error = anyhow::Error;

    fn try_from(value: DecodedFormat) -> Result<Self, Self::Error> {
        match value {
            DecodedFormat::NV12 => Ok(Format::NV12),
            DecodedFormat::I420 => Err(anyhow!("Unsupported format")),
        }
    }
}

impl TryFrom<Format> for DecodedFormat {
    type Error = anyhow::Error;

    fn try_from(value: Format) -> Result<Self, Self::Error> {
        match value {
            Format::NV12 => Ok(DecodedFormat::NV12),
            _ => Err(anyhow!("Unsupported format")),
        }
    }
}

impl TryFrom<libva::VAProfile::Type> for Profile {
    type Error = anyhow::Error;

    fn try_from(value: libva::VAProfile::Type) -> Result<Self, Self::Error> {
        match value {
            libva::VAProfile::VAProfileH264Baseline => Ok(Self::H264Baseline),
            libva::VAProfile::VAProfileH264Main => Ok(Self::H264Main),
            libva::VAProfile::VAProfileH264High => Ok(Self::H264High),
            libva::VAProfile::VAProfileH264StereoHigh => Ok(Self::H264StereoHigh),
            libva::VAProfile::VAProfileH264MultiviewHigh => Ok(Self::H264MultiviewHigh),
            libva::VAProfile::VAProfileHEVCMain => Ok(Self::HevcMain),
            libva::VAProfile::VAProfileHEVCMain10 => Ok(Self::HevcMain10),
            // (VP8Version0_3, VP8Profile0),
            libva::VAProfile::VAProfileVP9Profile0 => Ok(Self::VP9Profile0),
            libva::VAProfile::VAProfileVP9Profile1 => Ok(Self::VP9Profile1),
            libva::VAProfile::VAProfileVP9Profile2 => Ok(Self::VP9Profile2),
            libva::VAProfile::VAProfileVP9Profile3 => Ok(Self::VP9Profile3),
            _ => Err(anyhow!(
                "Conversion failed for unexpected VAProfile: {}",
                value
            )),
        }
    }
}

/// The state for the output queue containing the buffers that will receive the
/// decoded data.
enum OutputQueueState {
    /// Waiting for the client to call `set_output_buffer_count`.
    AwaitingBufferCount,
    /// Codec is capable of decoding frames.
    Decoding {
        /// The output queue which indirectly contains the output buffers given by crosvm
        output_queue: OutputQueue,
    },
    /// Dynamic Resolution Change - we can still accept buffers in the old
    /// format, but are waiting for new parameters before doing any decoding.
    Drc,
}

impl OutputQueueState {
    fn output_queue_mut(&mut self) -> Result<&mut OutputQueue> {
        match self {
            OutputQueueState::Decoding { output_queue } => Ok(output_queue),
            _ => Err(anyhow!("Invalid state")),
        }
    }
}

///A safe decoder abstraction over libva for a single vaContext
pub struct VaapiDecoder {
    /// The capabilities for the decoder
    caps: Capability,
}

// The VA capabilities for the coded side
struct CodedCap {
    profile: libva::VAProfile::Type,
    max_width: u32,
    max_height: u32,
    // bitmask containing the OR'd supported raw formats
    rt_fmts: u32,
}

// The VA capabilities for the raw side
struct RawCap {
    fourcc: u32,
    min_width: u32,
    min_height: u32,
    max_width: u32,
    max_height: u32,
}

impl VaapiDecoder {
    // Query the capabilities for the coded format
    fn get_coded_cap(
        display: &libva::Display,
        profile: libva::VAProfile::Type,
    ) -> Result<CodedCap> {
        let mut attrs = vec![
            libva::VAConfigAttrib {
                type_: libva::VAConfigAttribType::VAConfigAttribMaxPictureWidth,
                value: 0,
            },
            libva::VAConfigAttrib {
                type_: libva::VAConfigAttribType::VAConfigAttribMaxPictureHeight,
                value: 0,
            },
            libva::VAConfigAttrib {
                type_: libva::VAConfigAttribType::VAConfigAttribRTFormat,
                value: 0,
            },
        ];

        display.get_config_attributes(profile, libva::VAEntrypoint::VAEntrypointVLD, &mut attrs)?;

        let mut max_width = 1u32;
        let mut max_height = 1u32;
        let mut rt_fmts = 0u32;

        for attr in &attrs {
            if attr.value == libva::constants::VA_ATTRIB_NOT_SUPPORTED {
                continue;
            }

            match attr.type_ {
                libva::VAConfigAttribType::VAConfigAttribMaxPictureWidth => max_width = attr.value,
                libva::VAConfigAttribType::VAConfigAttribMaxPictureHeight => {
                    max_height = attr.value
                }
                libva::VAConfigAttribType::VAConfigAttribRTFormat => rt_fmts = attr.value,

                _ => panic!("Unexpected VAConfigAttribType {}", attr.type_),
            }
        }

        Ok(CodedCap {
            profile,
            max_width,
            max_height,
            rt_fmts,
        })
    }

    // Query the capabilities for the raw format
    fn get_raw_caps(display: Rc<libva::Display>, coded_cap: &CodedCap) -> Result<Vec<RawCap>> {
        const RT_FMTS: [u32; 18] = [
            libva::constants::VA_RT_FORMAT_YUV420,
            libva::constants::VA_RT_FORMAT_YUV422,
            libva::constants::VA_RT_FORMAT_YUV444,
            libva::constants::VA_RT_FORMAT_YUV411,
            libva::constants::VA_RT_FORMAT_YUV400,
            libva::constants::VA_RT_FORMAT_YUV420_10,
            libva::constants::VA_RT_FORMAT_YUV422_10,
            libva::constants::VA_RT_FORMAT_YUV444_10,
            libva::constants::VA_RT_FORMAT_YUV420_12,
            libva::constants::VA_RT_FORMAT_YUV422_12,
            libva::constants::VA_RT_FORMAT_YUV444_12,
            libva::constants::VA_RT_FORMAT_RGB16,
            libva::constants::VA_RT_FORMAT_RGB32,
            libva::constants::VA_RT_FORMAT_RGBP,
            libva::constants::VA_RT_FORMAT_RGB32_10,
            libva::constants::VA_RT_FORMAT_PROTECTED,
            libva::constants::VA_RT_FORMAT_RGB32_10BPP,
            libva::constants::VA_RT_FORMAT_YUV420_10BPP,
        ];

        let mut raw_caps = Vec::new();

        for rt_fmt in RT_FMTS {
            if coded_cap.rt_fmts & rt_fmt == 0 {
                continue;
            }

            // Create a config with this RTFormat, so we can query surface
            // attributes
            let attrs = vec![libva::VAConfigAttrib {
                type_: libva::VAConfigAttribType::VAConfigAttribRTFormat,
                value: rt_fmt,
            }];

            let mut config = display.create_config(
                Some(attrs),
                coded_cap.profile,
                libva::VAEntrypoint::VAEntrypointVLD,
            )?;

            let fourcc = match config
                .query_surface_attribute(libva::VASurfaceAttribType::VASurfaceAttribPixelFormat)?
            {
                Some(libva::GenericValue::Integer(i)) => i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => continue,
            };

            let min_width = match config
                .query_surface_attribute(libva::VASurfaceAttribType::VASurfaceAttribMinWidth)?
            {
                Some(libva::GenericValue::Integer(i)) => i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => 1,
            };

            let min_height = match config
                .query_surface_attribute(libva::VASurfaceAttribType::VASurfaceAttribMinHeight)?
            {
                Some(libva::GenericValue::Integer(i)) => i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => 1,
            };
            let max_width = match config
                .query_surface_attribute(libva::VASurfaceAttribType::VASurfaceAttribMaxWidth)?
            {
                Some(libva::GenericValue::Integer(i)) => i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => 1,
            };

            let max_height = match config
                .query_surface_attribute(libva::VASurfaceAttribType::VASurfaceAttribMaxHeight)?
            {
                Some(libva::GenericValue::Integer(i)) => i as u32,
                Some(other) => panic!("Unexpected VAGenericValue {:?}", other),
                None => 1,
            };

            raw_caps.push(RawCap {
                fourcc,
                min_width,
                min_height,
                max_width,
                max_height,
            });
        }

        Ok(raw_caps)
    }

    /// Creates a new instance of the Vaapi decoder.
    pub fn new() -> Result<Self> {
        let display = Rc::new(libva::Display::open()?);

        let va_profiles = display.query_config_profiles()?;

        let mut profiles = Vec::new();
        let mut in_fmts = Vec::new();
        let mut out_fmts = Vec::new();
        let mut profiles_map: BTreeMap<Format, Vec<Profile>> = Default::default();

        // VA has no API for querying the levels supported by the driver.
        // vaQueryProcessingRate is close, but not quite a solution here
        // for all codecs.
        let levels: BTreeMap<Format, Vec<Level>> = Default::default();

        for va_profile in va_profiles {
            let entrypoints = display.query_config_entrypoints(va_profile)?;
            if !entrypoints
                .iter()
                .any(|e| *e == libva::VAEntrypoint::VAEntrypointVLD)
            {
                // All formats we are aiming to support require
                // VAEntrypointVLD.
                continue;
            }
            // Manually push all VP8 profiles, since VA exposes only a single
            // VP8 profile for all of these
            if va_profile == libva::VAProfile::VAProfileVP8Version0_3 {
                profiles.push(Profile::VP8Profile0);
                profiles.push(Profile::VP8Profile1);
                profiles.push(Profile::VP8Profile2);
                profiles.push(Profile::VP8Profile3);
            }

            let profile = match Profile::try_from(va_profile) {
                Ok(p) => p,
                // Skip if we cannot convert to a valid virtio format
                Err(_) => continue,
            };

            let coded_cap = VaapiDecoder::get_coded_cap(display.as_ref(), va_profile)?;
            let raw_caps = VaapiDecoder::get_raw_caps(Rc::clone(&display), &coded_cap)?;

            let coded_frame_fmt = FrameFormat {
                width: FormatRange {
                    min: 1,
                    max: coded_cap.max_width,
                    step: 1,
                },

                height: FormatRange {
                    min: 1,
                    max: coded_cap.max_height,
                    step: 1,
                },

                bitrates: Default::default(),
            };

            let coded_format = profile.to_format();
            match profiles_map.entry(coded_format) {
                Entry::Vacant(e) => {
                    e.insert(vec![profile]);
                }
                Entry::Occupied(mut ps) => {
                    ps.get_mut().push(profile);
                }
            }

            let mut n_out = 0;
            for raw_cap in raw_caps {
                if raw_cap.fourcc != libva::constants::VA_FOURCC_NV12 {
                    // Apparently only NV12 is currently supported by virtio video
                    continue;
                }

                let raw_frame_fmt = FrameFormat {
                    width: FormatRange {
                        min: raw_cap.min_width,
                        max: raw_cap.max_width,
                        step: 1,
                    },

                    height: FormatRange {
                        min: raw_cap.min_height,
                        max: raw_cap.max_height,
                        step: 1,
                    },

                    bitrates: Default::default(),
                };

                out_fmts.push(FormatDesc {
                    mask: 0,
                    format: Format::NV12,
                    frame_formats: vec![raw_frame_fmt],
                    plane_align: 1,
                });

                n_out += 1;
            }

            in_fmts.push(FormatDesc {
                mask: !(u64::MAX << n_out) << (out_fmts.len() - n_out),
                format: coded_format,
                frame_formats: vec![coded_frame_fmt],
                plane_align: 1,
            });
        }

        Ok(Self {
            caps: Capability::new(in_fmts, out_fmts, profiles_map, levels),
        })
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    width: u32,
    height: u32,
}

/// A decoder session for the libva backend
pub struct VaapiDecoderSession {
    /// The implementation for the codec specific logic.
    codec: Box<dyn VideoDecoder>,
    /// The state for the output queue. Updated when `set_output_buffer_count`
    /// is called or when we detect a dynamic resolution change.
    output_queue_state: OutputQueueState,
    /// Queue containing decoded pictures.
    ready_queue: VecDeque<Box<dyn DynDecodedHandle>>,
    /// Queue containing the buffers we have not yet submitted to the codec.
    submit_queue: VecDeque<PendingJob>,
    /// The event queue we can use to signal new events.
    event_queue: EventQueue<DecoderEvent>,
    /// Whether the decoder is currently flushing.
    flushing: bool,
    /// The coded format we are decoding.
    coded_format: Format,
}

/// A convenience type implementing persistent slice access for BufferHandles.
pub struct BufferMapping<T: Borrow<U>, U: BufferHandle> {
    #[allow(dead_code)]
    /// The underlying resource. Must be kept so as not to drop the BufferHandle
    resource: T,
    /// The mapping that backs the underlying slices returned by AsRef and AsMut
    mapping: MemoryMappingArena,
    /// A phantom so that the U parameter is used.
    phantom: PhantomData<U>,
}

impl<T: Borrow<U>, U: BufferHandle> BufferMapping<T, U> {
    /// Creates a new BufferMap
    pub fn new(resource: T, offset: usize, size: usize) -> Result<Self> {
        let mapping = resource.borrow().get_mapping(offset, size)?;

        Ok(Self {
            resource,
            mapping,
            phantom: PhantomData,
        })
    }
}

impl<T: Borrow<U>, U: BufferHandle> AsRef<[u8]> for BufferMapping<T, U> {
    fn as_ref(&self) -> &[u8] {
        let mapping = &self.mapping;
        // Safe because the mapping is linear and we own it, so it will not be unmapped during
        // the lifetime of this slice.
        unsafe { std::slice::from_raw_parts(mapping.as_ptr(), mapping.size()) }
    }
}

impl<T: Borrow<U>, U: BufferHandle> AsMut<[u8]> for BufferMapping<T, U> {
    fn as_mut(&mut self) -> &mut [u8] {
        let mapping = &self.mapping;
        // Safe because the mapping is linear and we own it, so it will not be unmapped during
        // the lifetime of this slice.
        unsafe { std::slice::from_raw_parts_mut(mapping.as_ptr(), mapping.size()) }
    }
}

impl VaapiDecoderSession {
    fn change_resolution(&mut self, new_params: DrcParams) -> Result<()> {
        // Ask the client for new buffers.
        self.event_queue
            .queue_event(DecoderEvent::ProvidePictureBuffers {
                min_num_buffers: u32::try_from(new_params.min_num_buffers)?,
                width: new_params.width as i32,
                height: new_params.height as i32,
                visible_rect: new_params.visible_rect,
            })?;

        // Drop our output queue and wait for the new number of output buffers.
        self.output_queue_state = match &self.output_queue_state {
            // If this is part of the initialization step, then do not switch states.
            OutputQueueState::AwaitingBufferCount => OutputQueueState::AwaitingBufferCount,
            OutputQueueState::Decoding { .. } => OutputQueueState::Drc,
            _ => return Err(anyhow!("Invalid state during DRC.")),
        };

        Ok(())
    }

    /// Copy raw decoded data from `image` into the output buffer
    pub fn output_picture(&mut self, decoded_frame: &dyn DynDecodedHandle) -> Result<bool> {
        let output_queue = self.output_queue_state.output_queue_mut()?;

        // Output buffer to be used.
        let (picture_buffer_id, output_buffer) = match output_queue.try_get_ready_buffer() {
            Some(ready_buffer) => ready_buffer,
            None => {
                return Ok(false);
            }
        };

        let mapped_resolution = decoded_frame
            .dyn_picture_mut()
            .dyn_mappable_handle_mut()
            .mapped_resolution()?;

        let display_resolution = decoded_frame.display_resolution();

        // Get a mapping from the start of the buffer to the size of the
        // underlying decoded data in the Image.
        let mut output_map: BufferMapping<_, GuestResourceHandle> = BufferMapping::new(
            &mut output_buffer.handle,
            0,
            mapped_resolution.width as usize * mapped_resolution.height as usize * 3 / 2,
        )?;

        let output_bytes = output_map.as_mut();

        decoded_frame
            .dyn_picture_mut()
            .dyn_mappable_handle_mut()
            .read(output_bytes)?;

        let timestamp = decoded_frame.timestamp();
        let picture_buffer_id = picture_buffer_id as i32;

        // Say that we are done decoding this picture.
        self.event_queue
            .queue_event(DecoderEvent::PictureReady {
                picture_buffer_id,
                timestamp,
                visible_rect: Rect {
                    left: 0,
                    top: 0,
                    right: display_resolution.width as i32,
                    bottom: display_resolution.height as i32,
                },
            })
            .map_err(|e| {
                VideoError::BackendFailure(anyhow!("Can't queue the PictureReady event {}", e))
            })?;

        Ok(true)
    }

    fn drain_ready_queue(&mut self) -> Result<()> {
        while let Some(decoded_frame) = self.ready_queue.pop_front() {
            let outputted = self.output_picture(decoded_frame.as_ref())?;
            if !outputted {
                self.ready_queue.push_front(decoded_frame);
                break;
            }
        }

        Ok(())
    }

    fn try_emit_flush_completed(&mut self) -> Result<()> {
        let num_ready_remaining = self.ready_queue.len();
        let num_submit_remaining = self.submit_queue.len();

        if num_ready_remaining == 0 && num_submit_remaining == 0 {
            self.flushing = false;

            let event_queue = &mut self.event_queue;

            event_queue
                .queue_event(DecoderEvent::FlushCompleted(Ok(())))
                .map_err(|e| anyhow!("Can't queue the PictureReady event {}", e))
        } else {
            Ok(())
        }
    }

    fn decode_one_job(&mut self, job: PendingJob) -> VideoResult<()> {
        let PendingJob {
            resource_id,
            timestamp,
            resource,
            offset,
            bytes_used,
        } = job;

        let bitstream_map: BufferMapping<_, GuestResourceHandle> = BufferMapping::new(
            resource,
            offset.try_into().unwrap(),
            bytes_used.try_into().unwrap(),
        )
        .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?;

        let frames = self.codec.decode(timestamp, &bitstream_map);

        match frames {
            Ok(frames) => {
                self.event_queue
                    .queue_event(DecoderEvent::NotifyEndOfBitstreamBuffer(timestamp as u32))
                    .map_err(|e| {
                        VideoError::BackendFailure(anyhow!(
                            "Can't queue the NotifyEndOfBitstream event {}",
                            e
                        ))
                    })?;

                if self.codec.negotiation_possible() {
                    let resolution = self.codec.backend().coded_resolution().unwrap();

                    let drc_params = DrcParams {
                        min_num_buffers: self.codec.backend().num_resources_total(),
                        width: resolution.width,
                        height: resolution.height,
                        visible_rect: Rect {
                            left: 0,
                            top: 0,
                            right: resolution.width as i32,
                            bottom: resolution.height as i32,
                        },
                    };

                    self.change_resolution(drc_params)
                        .map_err(VideoError::BackendFailure)?;
                }

                for decoded_frame in frames {
                    self.ready_queue.push_back(decoded_frame);
                }

                self.drain_ready_queue()
                    .map_err(VideoError::BackendFailure)?;

                Ok(())
            }

            Err(e) => {
                let event_queue = &mut self.event_queue;

                event_queue
                    .queue_event(DecoderEvent::NotifyError(VideoError::BackendFailure(
                        anyhow!("Decoding buffer {} failed", resource_id),
                    )))
                    .map_err(|e| {
                        VideoError::BackendFailure(anyhow!(
                            "Can't queue the NotifyError event {}",
                            e
                        ))
                    })?;

                Err(VideoError::BackendFailure(anyhow!(e)))
            }
        }
    }

    fn drain_submit_queue(&mut self) -> VideoResult<()> {
        while let Some(queued_buffer) = self.submit_queue.pop_front() {
            match self.codec.num_resources_left() {
                Some(left) if left == 0 => {
                    self.submit_queue.push_front(queued_buffer);
                    break;
                }

                _ => self.decode_one_job(queued_buffer)?,
            }
        }

        Ok(())
    }
}

impl DecoderSession for VaapiDecoderSession {
    fn set_output_parameters(&mut self, buffer_count: usize, _: Format) -> VideoResult<()> {
        let output_queue_state = &mut self.output_queue_state;

        // This logic can still be improved, in particular it needs better
        // support at the virtio-video protocol level.
        //
        // We must ensure that set_output_parameters is only called after we are
        // sure that we have processed some stream metadata, which currently is
        // not the case. In particular, the {SET|GET}_PARAMS logic currently
        // takes place *before* we had a chance to parse any stream metadata at
        // all.
        //
        // This can lead to a situation where we accept a format (say, NV12),
        // but then discover we are unable to decode it after processing some
        // buffers (because the stream indicates that the bit depth is 10, for
        // example). Note that there is no way to reject said stream as of right
        // now unless we hardcode NV12 in cros-codecs itself.
        //
        // Nevertheless, the support is already in place in cros-codecs: the
        // decoders will queue buffers until they read some metadata. At this
        // point, it will allow for the negotiation of the decoded format until
        // a new call to decode() is made. At the crosvm level, we can use this
        // window of time to try different decoded formats with .try_format().
        //
        // For now, we accept the default format chosen by cros-codecs instead.
        // In practice, this means NV12 if it the stream can be decoded into
        // NV12 and if the hardware can do so.

        match output_queue_state {
            OutputQueueState::AwaitingBufferCount | OutputQueueState::Drc => {
                // Accept the default format chosen by cros-codecs instead.
                //
                // if let Some(backend_format) = self.backend.backend().format() {
                //     let backend_format = Format::try_from(backend_format);

                //     let format_matches = match backend_format {
                //         Ok(backend_format) => backend_format != format,
                //         Err(_) => false,
                //     };

                //     if !format_matches {
                //         let format =
                //             DecodedFormat::try_from(format).map_err(VideoError::BackendFailure)?;

                //         self.backend.backend().try_format(format).map_err(|e| {
                //             VideoError::BackendFailure(anyhow!(
                //                 "Failed to set the codec backend format: {}",
                //                 e
                //             ))
                //         })?;
                //     }
                // }

                *output_queue_state = OutputQueueState::Decoding {
                    output_queue: OutputQueue::new(buffer_count),
                };

                Ok(())
            }
            _ => Err(VideoError::BackendFailure(anyhow!(
                "Invalid state while calling set_output_parameters"
            ))),
        }
    }

    fn decode(
        &mut self,
        resource_id: u32,
        timestamp: u64,
        resource: GuestResourceHandle,
        offset: u32,
        bytes_used: u32,
    ) -> VideoResult<()> {
        let job = PendingJob {
            resource_id,
            timestamp,
            resource,
            offset,
            bytes_used,
        };

        self.submit_queue.push_back(job);
        self.drain_submit_queue()?;

        Ok(())
    }

    fn flush(&mut self) -> VideoResult<()> {
        self.flushing = true;

        self.drain_submit_queue()?;

        // Retrieve ready frames from the codec, if any.
        let pics = self
            .codec
            .flush()
            .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?;

        self.ready_queue.extend(pics);

        self.drain_ready_queue()
            .map_err(VideoError::BackendFailure)?;

        self.try_emit_flush_completed()
            .map_err(VideoError::BackendFailure)
    }

    fn reset(&mut self) -> VideoResult<()> {
        // Drop the queued output buffers.
        self.clear_output_buffers()?;

        // TODO(acourbot) IIUC we don't need to reset the resolution here, we can be optimistic
        // and assume that it won't change - if it does we will get a DRC event anyway.

        self.event_queue
            .queue_event(DecoderEvent::ResetCompleted(Ok(())))
            .map_err(|e| {
                VideoError::BackendFailure(anyhow!("Can't queue the ResetCompleted event {}", e))
            })?;

        let display = Rc::new(Display::open().map_err(VideoError::BackendFailure)?);
        let codec: Box<dyn VideoDecoder> = match self.coded_format {
            Format::VP8 => {
                let backend = Box::new(
                    cros_codecs::decoders::vp8::backends::stateless::vaapi::Backend::new(
                        Rc::clone(&display),
                    )
                    .unwrap(),
                );

                Box::new(
                    cros_codecs::decoders::vp8::decoder::Decoder::new(
                        backend,
                        cros_codecs::decoders::BlockingMode::NonBlocking,
                    )
                    .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
                )
            }

            Format::VP9 => {
                let backend = Box::new(
                    cros_codecs::decoders::vp9::backends::stateless::vaapi::Backend::new(
                        Rc::clone(&display),
                    )
                    .unwrap(),
                );

                Box::new(
                    cros_codecs::decoders::vp9::decoder::Decoder::new(
                        backend,
                        cros_codecs::decoders::BlockingMode::NonBlocking,
                    )
                    .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
                )
            }

            Format::H264 => {
                let backend = Box::new(
                    cros_codecs::decoders::h264::backends::stateless::vaapi::Backend::new(
                        Rc::clone(&display),
                    )
                    .unwrap(),
                );
                Box::new(
                    cros_codecs::decoders::h264::decoder::Decoder::new(
                        backend,
                        cros_codecs::decoders::BlockingMode::NonBlocking,
                    )
                    .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
                )
            }

            _ => return Err(VideoError::InvalidFormat),
        };

        *self = VaapiDecoderSession {
            codec,
            output_queue_state: OutputQueueState::AwaitingBufferCount,
            ready_queue: Default::default(),
            submit_queue: Default::default(),
            event_queue: EventQueue::new().map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
            flushing: Default::default(),
            coded_format: self.coded_format,
        };

        Ok(())
    }

    fn clear_output_buffers(&mut self) -> VideoResult<()> {
        // Cancel any ongoing flush.
        self.flushing = false;

        // Drop all output buffers we currently hold.
        if let OutputQueueState::Decoding { output_queue } = &mut self.output_queue_state {
            output_queue.clear_ready_buffers();
        }

        // Drop all decoded frames signaled as ready and cancel any reported flush.
        self.event_queue.retain(|event| {
            !matches!(
                event,
                DecoderEvent::PictureReady { .. } | DecoderEvent::FlushCompleted(_)
            )
        });

        Ok(())
    }

    fn event_pipe(&self) -> &dyn base::AsRawDescriptor {
        &self.event_queue
    }

    fn use_output_buffer(
        &mut self,
        picture_buffer_id: i32,
        resource: GuestResource,
    ) -> VideoResult<()> {
        let output_queue_state = &mut self.output_queue_state;
        if let OutputQueueState::Drc = output_queue_state {
            // Reusing buffers during DRC is valid, but we won't use them and can just drop them.
            return Ok(());
        }

        let output_queue = output_queue_state
            .output_queue_mut()
            .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?;

        // TODO: there's a type mismatch here between the trait and the signature for `import_buffer`
        output_queue
            .import_buffer(picture_buffer_id as u32, resource)
            .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?;

        self.drain_ready_queue()
            .map_err(VideoError::BackendFailure)?;

        Ok(())
    }

    fn reuse_output_buffer(&mut self, picture_buffer_id: i32) -> VideoResult<()> {
        let output_queue_state = &mut self.output_queue_state;
        if let OutputQueueState::Drc = output_queue_state {
            // Reusing buffers during DRC is valid, but we won't use them and can just drop them.
            return Ok(());
        }

        let output_queue = output_queue_state
            .output_queue_mut()
            .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?;

        // TODO: there's a type mismatch here between the trait and the signature for `import_buffer`
        output_queue
            .reuse_buffer(picture_buffer_id as u32)
            .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?;

        if self.flushing {
            // Try flushing again now that we have a new buffer. This might let
            // us progress further in the flush operation.
            self.flush()?;
        }
        Ok(())
    }

    fn read_event(&mut self) -> VideoResult<DecoderEvent> {
        self.event_queue
            .dequeue_event()
            .map_err(|e| VideoError::BackendFailure(anyhow!("Can't read event {}", e)))
    }
}

impl DecoderBackend for VaapiDecoder {
    type Session = VaapiDecoderSession;

    fn get_capabilities(&self) -> Capability {
        self.caps.clone()
    }

    fn new_session(&mut self, format: Format) -> VideoResult<Self::Session> {
        let display = Rc::new(Display::open().map_err(VideoError::BackendFailure)?);

        let codec: Box<dyn VideoDecoder> = match format {
            Format::VP8 => {
                let backend = Box::new(
                    cros_codecs::decoders::vp8::backends::stateless::vaapi::Backend::new(
                        Rc::clone(&display),
                    )
                    .unwrap(),
                );

                Box::new(
                    cros_codecs::decoders::vp8::decoder::Decoder::new(
                        backend,
                        cros_codecs::decoders::BlockingMode::NonBlocking,
                    )
                    .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
                )
            }

            Format::VP9 => {
                let backend = Box::new(
                    cros_codecs::decoders::vp9::backends::stateless::vaapi::Backend::new(
                        Rc::clone(&display),
                    )
                    .unwrap(),
                );

                Box::new(
                    cros_codecs::decoders::vp9::decoder::Decoder::new(
                        backend,
                        cros_codecs::decoders::BlockingMode::NonBlocking,
                    )
                    .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
                )
            }

            Format::H264 => {
                let backend = Box::new(
                    cros_codecs::decoders::h264::backends::stateless::vaapi::Backend::new(
                        Rc::clone(&display),
                    )
                    .unwrap(),
                );
                Box::new(
                    cros_codecs::decoders::h264::decoder::Decoder::new(
                        backend,
                        cros_codecs::decoders::BlockingMode::NonBlocking,
                    )
                    .map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
                )
            }
            _ => return Err(VideoError::InvalidFormat),
        };

        Ok(VaapiDecoderSession {
            codec,
            output_queue_state: OutputQueueState::AwaitingBufferCount,
            ready_queue: Default::default(),
            submit_queue: Default::default(),
            event_queue: EventQueue::new().map_err(|e| VideoError::BackendFailure(anyhow!(e)))?,
            flushing: Default::default(),
            coded_format: format,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Ignore this test by default as it requires libva-compatible hardware.
    #[ignore]
    fn test_get_capabilities() {
        let decoder = VaapiDecoder::new().unwrap();
        let caps = decoder.get_capabilities();
        assert!(!caps.input_formats().is_empty());
        assert!(!caps.output_formats().is_empty());
    }
}
