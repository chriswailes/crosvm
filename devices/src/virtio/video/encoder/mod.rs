// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the the `Encoder` struct, which is responsible for translation between the
//! virtio protocols and LibVDA APIs.

mod encoder;
mod libvda_encoder;

pub use encoder::EncoderError;
pub use libvda_encoder::LibvdaEncoder;

use base::{error, warn, WaitContext};
use std::collections::{BTreeMap, BTreeSet};

use crate::virtio::resource_bridge::{self, ResourceRequestSocket};
use crate::virtio::video::async_cmd_desc_map::AsyncCmdDescMap;
use crate::virtio::video::command::{QueueType, VideoCmd};
use crate::virtio::video::control::*;
use crate::virtio::video::device::VideoCmdResponseType;
use crate::virtio::video::device::{
    AsyncCmdResponse, AsyncCmdTag, Device, Token, VideoEvtResponseType,
};
use crate::virtio::video::encoder::encoder::{
    Encoder, EncoderEvent, EncoderSession, InputBufferId, OutputBufferId, SessionConfig,
    VideoFramePlane,
};
use crate::virtio::video::error::*;
use crate::virtio::video::event::{EvtType, VideoEvt};
use crate::virtio::video::format::{Format, Level, Profile};
use crate::virtio::video::params::Params;
use crate::virtio::video::protocol;
use crate::virtio::video::response::CmdResponse;

#[derive(Debug)]
struct QueuedInputResourceParams {
    encoder_id: InputBufferId,
    timestamp: u64,
    in_queue: bool,
}

struct InputResource {
    resource_handle: u128,
    planes: Vec<VideoFramePlane>,
    queue_params: Option<QueuedInputResourceParams>,
}

#[derive(Debug)]
struct QueuedOutputResourceParams {
    encoder_id: OutputBufferId,
    timestamp: u64,
    in_queue: bool,
}

struct OutputResource {
    resource_handle: u128,
    offset: u32,
    queue_params: Option<QueuedOutputResourceParams>,
}

#[derive(Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
enum PendingCommand {
    GetSrcParams,
    GetDstParams,
    Drain,
    SrcQueueClear,
    DstQueueClear,
}

struct Stream<T: EncoderSession> {
    id: u32,
    src_params: Params,
    dst_params: Params,
    dst_bitrate: u32,
    dst_profile: Profile,
    dst_h264_level: Option<Level>,

    encoder_session: Option<T>,
    received_input_buffers_event: bool,

    src_resources: BTreeMap<u32, InputResource>,
    encoder_input_buffer_ids: BTreeMap<InputBufferId, u32>,

    dst_resources: BTreeMap<u32, OutputResource>,
    encoder_output_buffer_ids: BTreeMap<OutputBufferId, u32>,

    pending_commands: BTreeSet<PendingCommand>,
    eos_notification_buffer: Option<OutputBufferId>,
}

impl<T: EncoderSession> Stream<T> {
    fn new<E: Encoder<Session = T>>(
        id: u32,
        desired_format: Format,
        encoder: &EncoderDevice<E>,
    ) -> VideoResult<Self> {
        const MIN_BUFFERS: u32 = 2;
        const MAX_BUFFERS: u32 = 342;
        const DEFAULT_WIDTH: u32 = 640;
        const DEFAULT_HEIGHT: u32 = 480;
        const DEFAULT_BITRATE: u32 = 6000;
        const DEFAULT_FPS: u32 = 30;

        let mut src_params = Params {
            min_buffers: MIN_BUFFERS,
            max_buffers: MAX_BUFFERS,
            ..Default::default()
        };

        let cros_capabilities = &encoder.cros_capabilities;

        cros_capabilities
            .populate_src_params(&mut src_params, Format::NV12, DEFAULT_WIDTH, DEFAULT_HEIGHT)
            .map_err(|_| VideoError::InvalidArgument)?;

        let mut dst_params = Default::default();

        // In order to support requesting encoder params change, we must know the default frame
        // rate, because VEA's request_encoding_params_change requires both framerate and
        // bitrate to be specified.
        cros_capabilities
            .populate_dst_params(
                &mut dst_params,
                &src_params,
                desired_format,
                /* fps= */ DEFAULT_FPS,
            )
            .map_err(|_| VideoError::InvalidArgument)?;
        // `format` is an Option since for the decoder, it is not populated until decoding has
        // started. for encoder, format should always be populated.
        let dest_format = dst_params
            .format
            .ok_or_else(|| VideoError::InvalidArgument)?;

        let dst_profile = cros_capabilities
            .get_default_profile(&dest_format)
            .ok_or_else(|| VideoError::InvalidArgument)?;

        let dst_h264_level = if dest_format == Format::H264 {
            Some(Level::H264_1_0)
        } else {
            None
        };

        Ok(Self {
            id,
            src_params,
            dst_params,
            dst_bitrate: DEFAULT_BITRATE,
            dst_profile,
            dst_h264_level,
            encoder_session: None,
            received_input_buffers_event: false,
            src_resources: Default::default(),
            encoder_input_buffer_ids: Default::default(),
            dst_resources: Default::default(),
            encoder_output_buffer_ids: Default::default(),
            pending_commands: Default::default(),
            eos_notification_buffer: None,
        })
    }

    fn has_encode_session(&self) -> bool {
        self.encoder_session.is_some()
    }

    fn set_encode_session<U: Encoder<Session = T>>(
        &mut self,
        encoder: &mut U,
        wait_ctx: &WaitContext<Token>,
    ) -> VideoResult<()> {
        if self.encoder_session.is_some() {
            error!(
                "stream {}: tried to add encode session when one already exists.",
                self.id
            );
            return Err(VideoError::InvalidOperation);
        }

        let new_session = encoder
            .start_session(SessionConfig {
                src_params: self.src_params.clone(),
                dst_params: self.dst_params.clone(),
                dst_profile: self.dst_profile,
                dst_bitrate: self.dst_bitrate,
                dst_h264_level: self.dst_h264_level.clone(),
            })
            .map_err(|_| VideoError::InvalidOperation)?;

        let event_pipe = new_session.event_pipe();

        wait_ctx
            .add(event_pipe, Token::Event { id: self.id })
            .map_err(|e| {
                error!(
                    "stream {}: failed to add FD to poll context: {}",
                    self.id, e
                );
                VideoError::InvalidOperation
            })?;
        self.encoder_session.replace(new_session);
        self.received_input_buffers_event = false;
        Ok(())
    }

    fn clear_encode_session(&mut self, wait_ctx: &WaitContext<Token>) -> VideoResult<()> {
        if let Some(session) = self.encoder_session.take() {
            let event_pipe = session.event_pipe();
            wait_ctx.delete(event_pipe).map_err(|e| {
                error!(
                    "stream: {}: failed to remove fd from poll context: {}",
                    self.id, e
                );
                VideoError::InvalidOperation
            })?;
        }
        Ok(())
    }

    fn require_input_buffers(
        &mut self,
        input_count: u32,
        input_frame_width: u32,
        input_frame_height: u32,
        output_buffer_size: u32,
    ) -> Option<Vec<VideoEvtResponseType>> {
        // TODO(alexlau): Does this always arrive after start_session,
        // but before the first encode call?
        // TODO(alexlau): set plane info from input_frame_width and input_frame_height
        self.src_params.min_buffers = input_count;
        self.src_params.max_buffers = 32;
        self.src_params.frame_width = input_frame_width;
        self.src_params.frame_height = input_frame_height;
        self.dst_params.plane_formats[0].plane_size = output_buffer_size;
        self.received_input_buffers_event = true;

        let mut responses = vec![];

        // Respond to any GetParams commands that were waiting.
        if self.pending_commands.remove(&PendingCommand::GetSrcParams) {
            responses.push(VideoEvtResponseType::AsyncCmd(
                AsyncCmdResponse::from_response(
                    AsyncCmdTag::GetParams {
                        stream_id: self.id,
                        queue_type: QueueType::Input,
                    },
                    CmdResponse::GetParams {
                        queue_type: QueueType::Input,
                        params: self.src_params.clone(),
                    },
                ),
            ));
        }
        if self.pending_commands.remove(&PendingCommand::GetDstParams) {
            responses.push(VideoEvtResponseType::AsyncCmd(
                AsyncCmdResponse::from_response(
                    AsyncCmdTag::GetParams {
                        stream_id: self.id,
                        queue_type: QueueType::Output,
                    },
                    CmdResponse::GetParams {
                        queue_type: QueueType::Output,
                        params: self.dst_params.clone(),
                    },
                ),
            ));
        }

        if responses.len() > 0 {
            Some(responses)
        } else {
            None
        }
    }

    fn processed_input_buffer(
        &mut self,
        input_buffer_id: InputBufferId,
    ) -> Option<Vec<VideoEvtResponseType>> {
        let resource_id = *match self.encoder_input_buffer_ids.get(&input_buffer_id) {
            Some(id) => id,
            None => {
                warn!("Received processed input buffer event for input buffer id {}, but missing resource, ResourceDestroyAll?", input_buffer_id);
                return None;
            }
        };

        let resource = match self.src_resources.get_mut(&resource_id) {
            Some(r) => r,
            None => {
                error!(
                    "Received processed input buffer event but missing resource with id {}",
                    resource_id
                );
                return None;
            }
        };

        let queue_params = match resource.queue_params.take() {
            Some(p) => p,
            None => {
                error!(
                    "Received processed input buffer event but resource with id {} was not queued.",
                    resource_id
                );
                return None;
            }
        };

        if !queue_params.in_queue {
            // A QueueClear command occurred after this buffer was queued.
            return None;
        }

        let tag = AsyncCmdTag::Queue {
            stream_id: self.id,
            queue_type: QueueType::Input,
            resource_id,
        };

        let resp = CmdResponse::ResourceQueue {
            timestamp: queue_params.timestamp,
            flags: 0,
            size: 0,
        };

        Some(vec![VideoEvtResponseType::AsyncCmd(
            AsyncCmdResponse::from_response(tag, resp),
        )])
    }

    fn processed_output_buffer(
        &mut self,
        output_buffer_id: OutputBufferId,
        bytesused: u32,
        timestamp: u64,
    ) -> Option<Vec<VideoEvtResponseType>> {
        let resource_id = *match self.encoder_output_buffer_ids.get(&output_buffer_id) {
            Some(id) => id,
            None => {
                warn!("Received processed output buffer event for output buffer id {}, but missing resource, ResourceDestroyAll?", output_buffer_id);
                return None;
            }
        };

        let resource = match self.dst_resources.get_mut(&resource_id) {
            Some(r) => r,
            None => {
                error!(
                    "Received processed output buffer event but missing resource with id {}",
                    resource_id
                );
                return None;
            }
        };

        let queue_params = match resource.queue_params.take() {
            Some(p) => p,
            None => {
                error!("Received processed output buffer event but resource with id {} was not queued.", resource_id);
                return None;
            }
        };

        if !queue_params.in_queue {
            // A QueueClear command occurred after this buffer was queued.
            return None;
        }

        let tag = AsyncCmdTag::Queue {
            stream_id: self.id,
            queue_type: QueueType::Output,
            resource_id,
        };

        let resp = CmdResponse::ResourceQueue {
            timestamp,
            // At the moment, a buffer is saved in `eos_notification_buffer`, and
            // the EOS flag is populated and returned after a flush() command.
            // TODO(b/149725148): Populate flags once libvda supports it.
            flags: 0,
            size: bytesused,
        };

        Some(vec![VideoEvtResponseType::AsyncCmd(
            AsyncCmdResponse::from_response(tag, resp),
        )])
    }

    fn flush_response(&mut self, flush_done: bool) -> Option<Vec<VideoEvtResponseType>> {
        let command_response = if flush_done {
            CmdResponse::NoData
        } else {
            error!("Flush could not be completed for stream {}", self.id);
            VideoError::InvalidOperation.into()
        };

        let mut async_responses = vec![];

        let eos_resource_id = match self.eos_notification_buffer {
            Some(r) => r,
            None => {
                error!(
                    "No EOS resource available on successful flush response (stream id {})",
                    self.id
                );
                return Some(vec![VideoEvtResponseType::Event(VideoEvt {
                    typ: EvtType::Error,
                    stream_id: self.id,
                })]);
            }
        };

        let eos_tag = AsyncCmdTag::Queue {
            stream_id: self.id,
            queue_type: QueueType::Output,
            resource_id: eos_resource_id,
        };

        let eos_response = CmdResponse::ResourceQueue {
            timestamp: 0,
            flags: protocol::VIRTIO_VIDEO_BUFFER_FLAG_EOS,
            size: 0,
        };

        async_responses.push(VideoEvtResponseType::AsyncCmd(
            AsyncCmdResponse::from_response(eos_tag, eos_response),
        ));

        if self.pending_commands.remove(&PendingCommand::Drain) {
            async_responses.push(VideoEvtResponseType::AsyncCmd(
                AsyncCmdResponse::from_response(
                    AsyncCmdTag::Drain { stream_id: self.id },
                    command_response.clone(),
                ),
            ));
        }

        if self.pending_commands.remove(&PendingCommand::SrcQueueClear) {
            async_responses.push(VideoEvtResponseType::AsyncCmd(
                AsyncCmdResponse::from_response(
                    AsyncCmdTag::Clear {
                        stream_id: self.id,
                        queue_type: QueueType::Input,
                    },
                    command_response.clone(),
                ),
            ));
        }

        if self.pending_commands.remove(&PendingCommand::DstQueueClear) {
            async_responses.push(VideoEvtResponseType::AsyncCmd(
                AsyncCmdResponse::from_response(
                    AsyncCmdTag::Clear {
                        stream_id: self.id,
                        queue_type: QueueType::Output,
                    },
                    command_response,
                ),
            ));
        }

        if async_responses.is_empty() {
            error!("Received flush response but there are no pending commands.");
            None
        } else {
            Some(async_responses)
        }
    }

    fn notify_error(&self, error: EncoderError) -> Option<Vec<VideoEvtResponseType>> {
        error!(
            "Received encoder error event for stream {}: {}",
            self.id, error
        );
        Some(vec![VideoEvtResponseType::Event(VideoEvt {
            typ: EvtType::Error,
            stream_id: self.id,
        })])
    }
}

pub struct EncoderDevice<T: Encoder> {
    cros_capabilities: encoder::EncoderCapabilities,
    encoder: T,
    streams: BTreeMap<u32, Stream<T::Session>>,
}

impl<T: Encoder> EncoderDevice<T> {
    pub fn new(encoder: T) -> encoder::Result<Self> {
        Ok(Self {
            cros_capabilities: encoder.query_capabilities()?,
            encoder,
            streams: Default::default(),
        })
    }

    fn query_capabilities(&self, queue_type: QueueType) -> VideoResult<VideoCmdResponseType> {
        let descs = match queue_type {
            QueueType::Input => self.cros_capabilities.input_format_descs.clone(),
            QueueType::Output => self.cros_capabilities.output_format_descs.clone(),
        };
        Ok(VideoCmdResponseType::Sync(CmdResponse::QueryCapability(
            descs,
        )))
    }

    fn stream_create(
        &mut self,
        stream_id: u32,
        desired_format: Format,
    ) -> VideoResult<VideoCmdResponseType> {
        if self.streams.contains_key(&stream_id) {
            return Err(VideoError::InvalidStreamId(stream_id));
        }
        let new_stream = Stream::new(stream_id, desired_format, self)?;

        self.streams.insert(stream_id, new_stream);
        Ok(VideoCmdResponseType::Sync(CmdResponse::NoData))
    }

    fn stream_destroy(&mut self, stream_id: u32) -> VideoResult<VideoCmdResponseType> {
        let mut stream = self
            .streams
            .remove(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;
        // TODO(alexlau): Handle resources that have been queued.
        if let Some(session) = stream.encoder_session.take() {
            if let Err(e) = self.encoder.stop_session(session) {
                error!("Failed to stop encode session {}: {}", stream_id, e);
            }
        }
        Ok(VideoCmdResponseType::Sync(CmdResponse::NoData))
    }

    fn stream_drain(&mut self, stream_id: u32) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;
        match stream.encoder_session {
            Some(ref mut session) => {
                if stream.pending_commands.contains(&PendingCommand::Drain) {
                    error!("A pending Drain command already exists.");
                    return Err(VideoError::InvalidOperation);
                }
                stream.pending_commands.insert(PendingCommand::Drain);

                if !stream
                    .pending_commands
                    .contains(&PendingCommand::SrcQueueClear)
                    && !stream
                        .pending_commands
                        .contains(&PendingCommand::DstQueueClear)
                {
                    // If a source or dest QueueClear is underway, a flush has
                    // already been sent.
                    if let Err(e) = session.flush() {
                        error!("Flush failed for stream id {}: {}", stream_id, e);
                    }
                }
                Ok(VideoCmdResponseType::Async(AsyncCmdTag::Drain {
                    stream_id,
                }))
            }
            None => {
                // Return an OK response since nothing has been queued yet.
                Ok(VideoCmdResponseType::Sync(CmdResponse::NoData))
            }
        }
    }

    fn resource_create(
        &mut self,
        wait_ctx: &WaitContext<Token>,
        resource_bridge: &ResourceRequestSocket,
        stream_id: u32,
        queue_type: QueueType,
        resource_id: u32,
        plane_offsets: Vec<u32>,
        uuid: u128,
    ) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;

        if !stream.has_encode_session() {
            // No encode session would have been created upon the first
            // QBUF if there was no previous S_FMT call.
            stream.set_encode_session(&mut self.encoder, wait_ctx)?;
        }

        let num_planes = plane_offsets.len();

        match queue_type {
            QueueType::Input => {
                if num_planes != stream.src_params.plane_formats.len() {
                    return Err(VideoError::InvalidParameter);
                }

                if stream.src_resources.contains_key(&resource_id) {
                    warn!("Replacing source resource with id {}", resource_id);
                }

                let resource_info =
                    resource_bridge::get_resource_info(resource_bridge, uuid as u32)
                        .map_err(VideoError::ResourceBridgeFailure)?;

                let planes: Vec<VideoFramePlane> = resource_info.planes[0..num_planes]
                    .into_iter()
                    .map(|plane_info| VideoFramePlane {
                        offset: plane_info.offset as usize,
                        stride: plane_info.stride as usize,
                    })
                    .collect();

                stream.src_resources.insert(
                    resource_id,
                    InputResource {
                        resource_handle: uuid,
                        planes,
                        queue_params: None,
                    },
                );
            }
            QueueType::Output => {
                if num_planes != stream.dst_params.plane_formats.len() {
                    return Err(VideoError::InvalidParameter);
                }

                if stream.dst_resources.contains_key(&resource_id) {
                    warn!("Replacing dest resource with id {}", resource_id);
                }

                let offset = plane_offsets[0];
                stream.dst_resources.insert(
                    resource_id,
                    OutputResource {
                        resource_handle: uuid,
                        offset,
                        queue_params: None,
                    },
                );
            }
        }

        Ok(VideoCmdResponseType::Sync(CmdResponse::NoData))
    }

    fn resource_queue(
        &mut self,
        resource_bridge: &ResourceRequestSocket,
        stream_id: u32,
        queue_type: QueueType,
        resource_id: u32,
        timestamp: u64,
        data_sizes: Vec<u32>,
    ) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;

        let encoder_session = match stream.encoder_session {
            Some(ref mut e) => e,
            None => {
                // The encoder session is created on the first ResourceCreate,
                // so it should exist here.
                error!("Encoder session did not exist at resource_queue.");
                return Err(VideoError::InvalidOperation);
            }
        };

        match queue_type {
            QueueType::Input => {
                if data_sizes.len() != stream.src_params.plane_formats.len() {
                    return Err(VideoError::InvalidParameter);
                }

                let src_resource = stream.src_resources.get_mut(&resource_id).ok_or(
                    VideoError::InvalidResourceId {
                        stream_id,
                        resource_id,
                    },
                )?;

                let resource_info = resource_bridge::get_resource_info(
                    resource_bridge,
                    src_resource.resource_handle as u32,
                )
                .map_err(VideoError::ResourceBridgeFailure)?;

                // TODO(alexlau): Figure out what to do with force_keyframe.
                // Perhaps by something in the protocol that allows the user
                // to specify the VIRTIO_VIDEO_BUFFER_FLAG_[IPB]FRAME constants?
                match encoder_session.encode(
                    resource_info.file,
                    &src_resource.planes,
                    timestamp,
                    /* force_keyframe= */ false,
                ) {
                    Ok(input_buffer_id) => {
                        if let Some(last_resource_id) = stream
                            .encoder_input_buffer_ids
                            .insert(input_buffer_id, resource_id)
                        {
                            error!(
                                "encoder input id {} was already mapped to resource id {}",
                                input_buffer_id, last_resource_id
                            );
                            return Err(VideoError::InvalidOperation);
                        }
                        let queue_params = QueuedInputResourceParams {
                            encoder_id: input_buffer_id,
                            timestamp,
                            in_queue: true,
                        };
                        if let Some(last_queue_params) =
                            src_resource.queue_params.replace(queue_params)
                        {
                            if last_queue_params.in_queue {
                                error!(
                                    "resource {} was already queued ({:?})",
                                    resource_id, last_queue_params
                                );
                                return Err(VideoError::InvalidOperation);
                            }
                        }
                    }
                    Err(e) => {
                        // TODO(alexlau): Return the actual error
                        error!("encode failed: {}", e);
                        return Err(VideoError::InvalidOperation);
                    }
                }
                Ok(VideoCmdResponseType::Async(AsyncCmdTag::Queue {
                    stream_id,
                    queue_type: QueueType::Input,
                    resource_id,
                }))
            }
            QueueType::Output => {
                if data_sizes.len() != stream.dst_params.plane_formats.len() {
                    return Err(VideoError::InvalidParameter);
                }

                let dst_resource = stream.dst_resources.get_mut(&resource_id).ok_or(
                    VideoError::InvalidResourceId {
                        stream_id,
                        resource_id,
                    },
                )?;

                let resource_info = resource_bridge::get_resource_info(
                    resource_bridge,
                    dst_resource.resource_handle as u32,
                )
                .map_err(VideoError::ResourceBridgeFailure)?;

                let mut buffer_size = data_sizes[0];

                // It seems that data_sizes[0] is 0 here. For now, take the stride
                // from resource_info instead because we're always allocating <size> x 1
                // blobs..
                // TODO(alexlau): Figure out how to fix this.
                if buffer_size == 0 {
                    buffer_size = resource_info.planes[0].offset + resource_info.planes[0].stride;
                }

                // Stores an output buffer to notify EOS.
                // This is necessary because libvda is unable to indicate EOS along with returned buffers.
                // For now, when a `Flush()` completes, this saved resource will be returned as a zero-sized
                // buffer with the EOS flag.
                if stream.eos_notification_buffer.is_none() {
                    stream.eos_notification_buffer = Some(resource_id);
                    return Ok(VideoCmdResponseType::Async(AsyncCmdTag::Queue {
                        stream_id,
                        queue_type: QueueType::Output,
                        resource_id,
                    }));
                }

                match encoder_session.use_output_buffer(
                    resource_info.file,
                    dst_resource.offset,
                    buffer_size,
                ) {
                    Ok(output_buffer_id) => {
                        if let Some(last_resource_id) = stream
                            .encoder_output_buffer_ids
                            .insert(output_buffer_id, resource_id)
                        {
                            error!(
                                "encoder output id {} was already mapped to resource id {}",
                                output_buffer_id, last_resource_id
                            );
                        }
                        let queue_params = QueuedOutputResourceParams {
                            encoder_id: output_buffer_id,
                            timestamp,
                            in_queue: true,
                        };
                        if let Some(last_queue_params) =
                            dst_resource.queue_params.replace(queue_params)
                        {
                            if last_queue_params.in_queue {
                                error!(
                                    "resource {} was already queued ({:?})",
                                    resource_id, last_queue_params
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!("use_output_buffer failed: {}", e);
                        return Err(VideoError::InvalidOperation);
                    }
                }
                Ok(VideoCmdResponseType::Async(AsyncCmdTag::Queue {
                    stream_id,
                    queue_type: QueueType::Output,
                    resource_id,
                }))
            }
        }
    }

    fn resource_destroy_all(&mut self, stream_id: u32) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;
        stream.src_resources.clear();
        stream.encoder_input_buffer_ids.clear();
        stream.dst_resources.clear();
        stream.encoder_output_buffer_ids.clear();
        stream.eos_notification_buffer.take();
        Ok(VideoCmdResponseType::Sync(CmdResponse::NoData))
    }

    fn queue_clear(
        &mut self,
        stream_id: u32,
        queue_type: QueueType,
    ) -> VideoResult<VideoCmdResponseType> {
        // Unfortunately, there is no way to clear the queue with VEA.
        // VDA has Reset() which also isn't done on a per-queue basis,
        // but VEA has no such API.
        // Doing a Flush() here and waiting for the flush response is also
        // not an option, because the virtio-video driver expects a prompt
        // response (search for "timed out waiting for queue clear" in
        // virtio_video_enc.c).
        // So for now, we do a Flush(), but also mark each currently
        // queued resource as no longer `in_queue`, and skip them when they
        // are returned.
        // TODO(b/153406792): Support per-queue clearing.
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;

        match queue_type {
            QueueType::Input => {
                for src_resource in stream.src_resources.values_mut() {
                    if let Some(ref mut queue_params) = src_resource.queue_params {
                        queue_params.in_queue = false;
                    }
                }
            }
            QueueType::Output => {
                for dst_resource in stream.dst_resources.values_mut() {
                    if let Some(ref mut queue_params) = dst_resource.queue_params {
                        queue_params.in_queue = false;
                    }
                }
                stream.eos_notification_buffer = None;
            }
        }
        Ok(VideoCmdResponseType::Sync(CmdResponse::NoData))
    }

    fn get_params(
        &mut self,
        stream_id: u32,
        queue_type: QueueType,
    ) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;

        if stream.encoder_session.is_some() && !stream.received_input_buffers_event {
            // If we haven't yet received an RequireInputBuffers
            // event, we need to wait for that before replying so that
            // the G_FMT response has the correct data.
            let pending_command = match queue_type {
                QueueType::Input => PendingCommand::GetSrcParams,
                QueueType::Output => PendingCommand::GetDstParams,
            };

            if !stream.pending_commands.insert(pending_command) {
                // There is already a G_FMT call waiting.
                error!("Pending get params call already exists.");
                return Err(VideoError::InvalidOperation);
            }

            Ok(VideoCmdResponseType::Async(AsyncCmdTag::GetParams {
                stream_id,
                queue_type,
            }))
        } else {
            let params = match queue_type {
                QueueType::Input => stream.src_params.clone(),
                QueueType::Output => stream.dst_params.clone(),
            };
            Ok(VideoCmdResponseType::Sync(CmdResponse::GetParams {
                queue_type,
                params,
            }))
        }
    }

    fn set_params(
        &mut self,
        wait_ctx: &WaitContext<Token>,
        stream_id: u32,
        queue_type: QueueType,
        format: Option<Format>,
        frame_width: u32,
        frame_height: u32,
        frame_rate: u32,
    ) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;

        if stream.src_resources.len() > 0 || stream.dst_resources.len() > 0 {
            // Buffers have already been queued and encoding has already started.
            return Err(VideoError::InvalidOperation);
        }

        match queue_type {
            QueueType::Input => {
                let desired_format = format.or(stream.src_params.format).unwrap_or(Format::NV12);
                self.cros_capabilities
                    .populate_src_params(
                        &mut stream.src_params,
                        desired_format,
                        frame_width,
                        frame_height,
                    )
                    .map_err(VideoError::EncoderImpl)?;
            }
            QueueType::Output => {
                let desired_format = format.or(stream.dst_params.format).unwrap_or(Format::H264);
                self.cros_capabilities
                    .populate_dst_params(
                        &mut stream.dst_params,
                        &stream.src_params,
                        desired_format,
                        frame_rate,
                    )
                    .map_err(VideoError::EncoderImpl)?;

                // Format is always populated for encoder.
                let new_format = stream
                    .dst_params
                    .format
                    .ok_or_else(|| VideoError::InvalidArgument)?;

                // If the selected profile no longer corresponds to the selected coded format,
                // reset it.
                stream.dst_profile = self
                    .cros_capabilities
                    .get_default_profile(&new_format)
                    .ok_or_else(|| VideoError::InvalidArgument)?;

                if new_format == Format::H264 {
                    stream.dst_h264_level = Some(Level::H264_1_0);
                } else {
                    stream.dst_h264_level = None;
                }
            }
        }

        // An encoder session has to be created immediately upon a SetParams
        // (S_FMT) call, because we need to receive the RequireInputBuffers
        // callback which has output buffer size info, in order to populate
        // dst_params to have the correct size on subsequent GetParams (G_FMT) calls.
        if stream.encoder_session.is_some() {
            stream.clear_encode_session(wait_ctx)?;
            if !stream.received_input_buffers_event {
                // This could happen if two SetParams calls are occuring at the same time.
                // For example, the user calls SetParams for the input queue on one thread,
                // and a new encode session is created. Then on another thread, SetParams
                // is called for the output queue before the first SetParams call has returned.
                // At this point, there is a new EncodeSession being created that has not
                // yet received a RequireInputBuffers event.
                // Even if we clear the encoder session and recreate it, this case
                // is handled because stream.pending_commands will still contain
                // the waiting GetParams responses, which will then receive fresh data once
                // the new session's RequireInputBuffers event happens.
                warn!("New encoder session being created while waiting for RequireInputBuffers.")
            }
        }
        stream.set_encode_session(&mut self.encoder, wait_ctx)?;
        Ok(VideoCmdResponseType::Sync(CmdResponse::NoData))
    }

    fn query_control(&self, query_ctrl_type: QueryCtrlType) -> VideoResult<VideoCmdResponseType> {
        let query_ctrl_response = match query_ctrl_type {
            QueryCtrlType::Profile(format) => match self.cros_capabilities.get_profiles(&format) {
                Some(profiles) => QueryCtrlResponse::Profile(profiles.clone()),
                None => {
                    return Err(VideoError::UnsupportedControl(CtrlType::Profile));
                }
            },
            QueryCtrlType::Level(format) => {
                match format {
                    Format::H264 => {
                        // TODO(alexlau): Figure out valid range of values.
                        QueryCtrlResponse::Level(vec![Level::H264_1_0])
                    }
                    _ => {
                        // Levels are only supported for H264.
                        return Err(VideoError::UnsupportedControl(CtrlType::Level));
                    }
                }
            }
        };

        Ok(VideoCmdResponseType::Sync(CmdResponse::QueryControl(
            query_ctrl_response,
        )))
    }

    fn get_control(
        &self,
        stream_id: u32,
        ctrl_type: CtrlType,
    ) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;
        let ctrl_val = match ctrl_type {
            CtrlType::Bitrate => CtrlVal::Bitrate(stream.dst_bitrate),
            CtrlType::Profile => CtrlVal::Profile(stream.dst_profile),
            CtrlType::Level => {
                let format = stream
                    .dst_params
                    .format
                    .ok_or_else(|| VideoError::InvalidArgument)?;
                match format {
                    Format::H264 => CtrlVal::Level(stream.dst_h264_level.ok_or_else(|| {
                        error!("H264 level not set");
                        VideoError::InvalidArgument
                    })?),
                    _ => {
                        return Err(VideoError::UnsupportedControl(ctrl_type));
                    }
                }
            }
        };
        Ok(VideoCmdResponseType::Sync(CmdResponse::GetControl(
            ctrl_val,
        )))
    }

    fn set_control(
        &mut self,
        stream_id: u32,
        ctrl_val: CtrlVal,
    ) -> VideoResult<VideoCmdResponseType> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(VideoError::InvalidStreamId(stream_id))?;
        match ctrl_val {
            CtrlVal::Bitrate(bitrate) => {
                if let Some(ref mut encoder_session) = stream.encoder_session {
                    if let Err(e) = encoder_session.request_encoding_params_change(
                        stream.dst_bitrate,
                        stream.dst_params.frame_rate,
                    ) {
                        error!(
                            "failed to dynamically request encoding params change: {}",
                            e
                        );
                        return Err(VideoError::InvalidOperation);
                    }
                }
                stream.dst_bitrate = bitrate;
            }
            CtrlVal::Profile(profile) => {
                if stream.encoder_session.is_some() {
                    // TODO(alexlau): If no resources have yet been queued,
                    // should the encoder session be recreated with the new
                    // desired level?
                    error!("set control called for profile but encoder session already exists.");
                    return Err(VideoError::InvalidOperation);
                }
                let format = stream
                    .dst_params
                    .format
                    .ok_or_else(|| VideoError::InvalidArgument)?;
                if format != profile.to_format() {
                    error!(
                        "specified profile does not correspond to the selected format ({})",
                        format
                    );
                    return Err(VideoError::InvalidOperation);
                }
                stream.dst_profile = profile;
            }
            CtrlVal::Level(level) => {
                if stream.encoder_session.is_some() {
                    // TODO(alexlau): If no resources have yet been queued,
                    // should the encoder session be recreated with the new
                    // desired level?
                    error!("set control called for level but encoder session already exists.");
                    return Err(VideoError::InvalidOperation);
                }
                let format = stream
                    .dst_params
                    .format
                    .ok_or_else(|| VideoError::InvalidArgument)?;
                if format != Format::H264 {
                    error!(
                        "set control called for level but format is not H264 ({})",
                        format
                    );
                    return Err(VideoError::InvalidOperation);
                }
                stream.dst_h264_level = Some(level);
            }
        }
        Ok(VideoCmdResponseType::Sync(CmdResponse::SetControl))
    }
}

impl<T: Encoder> Device for EncoderDevice<T> {
    fn process_cmd(
        &mut self,
        req: VideoCmd,
        wait_ctx: &WaitContext<Token>,
        resource_bridge: &ResourceRequestSocket,
    ) -> VideoResult<VideoCmdResponseType> {
        match req {
            VideoCmd::QueryCapability { queue_type } => self.query_capabilities(queue_type),
            VideoCmd::StreamCreate {
                stream_id,
                coded_format: desired_format,
            } => self.stream_create(stream_id, desired_format),
            VideoCmd::StreamDestroy { stream_id } => self.stream_destroy(stream_id),
            VideoCmd::StreamDrain { stream_id } => self.stream_drain(stream_id),
            VideoCmd::ResourceCreate {
                stream_id,
                queue_type,
                resource_id,
                plane_offsets,
                uuid,
            } => self.resource_create(
                wait_ctx,
                resource_bridge,
                stream_id,
                queue_type,
                resource_id,
                plane_offsets,
                uuid,
            ),
            VideoCmd::ResourceQueue {
                stream_id,
                queue_type,
                resource_id,
                timestamp,
                data_sizes,
            } => self.resource_queue(
                resource_bridge,
                stream_id,
                queue_type,
                resource_id,
                timestamp,
                data_sizes,
            ),
            VideoCmd::ResourceDestroyAll { stream_id, .. } => self.resource_destroy_all(stream_id),
            VideoCmd::QueueClear {
                stream_id,
                queue_type,
            } => self.queue_clear(stream_id, queue_type),
            VideoCmd::GetParams {
                stream_id,
                queue_type,
            } => self.get_params(stream_id, queue_type),
            VideoCmd::SetParams {
                stream_id,
                queue_type,
                params:
                    Params {
                        format,
                        frame_width,
                        frame_height,
                        frame_rate,
                        ..
                    },
            } => self.set_params(
                wait_ctx,
                stream_id,
                queue_type,
                format,
                frame_width,
                frame_height,
                frame_rate,
            ),
            VideoCmd::QueryControl { query_ctrl_type } => self.query_control(query_ctrl_type),
            VideoCmd::GetControl {
                stream_id,
                ctrl_type,
            } => self.get_control(stream_id, ctrl_type),
            VideoCmd::SetControl {
                stream_id,
                ctrl_val,
            } => self.set_control(stream_id, ctrl_val),
        }
    }

    fn process_event(
        &mut self,
        _desc_map: &mut AsyncCmdDescMap,
        stream_id: u32,
    ) -> Option<Vec<VideoEvtResponseType>> {
        let stream = match self.streams.get_mut(&stream_id) {
            Some(s) => s,
            None => {
                // TODO: remove fd from poll context?
                error!("Received event for missing stream id {}", stream_id);
                return None;
            }
        };

        let encoder_session = match stream.encoder_session {
            Some(ref mut s) => s,
            None => {
                error!(
                    "Received event for missing encoder session of stream id {}",
                    stream_id
                );
                return None;
            }
        };

        let event = match encoder_session.read_event() {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to read event for stream id {}: {}", stream_id, e);
                return None;
            }
        };

        match event {
            EncoderEvent::RequireInputBuffers {
                input_count,
                input_frame_width,
                input_frame_height,
                output_buffer_size,
            } => stream.require_input_buffers(
                input_count,
                input_frame_width,
                input_frame_height,
                output_buffer_size,
            ),
            EncoderEvent::ProcessedInputBuffer {
                id: input_buffer_id,
            } => stream.processed_input_buffer(input_buffer_id),
            EncoderEvent::ProcessedOutputBuffer {
                id: output_buffer_id,
                bytesused,
                timestamp,
            } => stream.processed_output_buffer(output_buffer_id, bytesused, timestamp),
            EncoderEvent::FlushResponse { flush_done } => stream.flush_response(flush_done),
            EncoderEvent::NotifyError { error } => stream.notify_error(error),
        }
    }
}
