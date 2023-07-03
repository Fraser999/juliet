//! `juliet` IO layer
//!
//! The IO layer combines a lower-level transport like a TCP Stream with the
//! [`JulietProtocol`](crate::juliet::JulietProtocol) protocol implementation and some memory buffer
//! to provide a working high-level transport for juliet messages. It allows users of this layer to
//! send messages across over multiple channels, without having to worry about frame multiplexing or
//! request limits.
//!
//! The layer is designed to run in its own task, with handles to allow sending messages in, or
//! receiving them as they arrive.

use std::{
    collections::VecDeque,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Notify,
};

use crate::{
    header::Header,
    protocol::{
        CompletedRead, FrameIter, JulietProtocol, LocalProtocolViolation, OutgoingFrame,
        OutgoingMessage,
    },
    ChannelId, Id, Outcome,
};

#[derive(Debug)]
enum QueuedItem {
    Request { payload: Option<Bytes> },
    Response { id: Id, payload: Option<Bytes> },
    RequestCancellation { id: Id },
    ResponseCancellation { id: Id },
    Error { id: Id, payload: Bytes },
}

impl QueuedItem {
    #[inline(always)]
    fn is_request(&self) -> bool {
        matches!(self, QueuedItem::Request { .. })
    }

    fn is_multi_frame(&self, max_frame_size: u32) -> bool {
        todo!()
    }
}

/// [`IoCore`] error.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Failed to read from underlying reader.
    #[error("read failed")]
    ReadFailed(#[source] io::Error),
    /// Failed to write using underlying writer.
    #[error("write failed")]
    WriteFailed(#[source] io::Error),
    #[error("local protocol violation")]
    /// Local protocol violation - caller violated the crate's API.
    LocalProtocolViolation(#[from] LocalProtocolViolation),
}

pub struct IoCore<const N: usize, R, W> {
    /// The actual protocol state.
    juliet: JulietProtocol<N>,

    /// Underlying transport, reader.
    reader: R,
    /// Underlying transport, writer.
    writer: W,
    /// Read buffer for incoming data.
    buffer: BytesMut,

    /// The frame in the process of being sent, maybe be partially transferred.
    current_frame: Option<OutgoingFrame>,
    /// The header of the current multi-frame transfer.
    active_multi_frame: [Option<Header>; N],
    /// Frames that can be sent next.
    ready_queue: VecDeque<FrameIter>,

    /// Shared data across handles and core.
    shared: Arc<IoShared<N>>,
}

struct IoShared<const N: usize> {
    /// Messages queued that are not yet ready to send.
    wait_queue: [Mutex<VecDeque<QueuedItem>>; N],
    /// Number of requests already buffered per channel.
    requests_buffered: [AtomicUsize; N],
    /// Maximum allowed number of requests to buffer per channel.
    requests_limit: [usize; N],
}

impl<const N: usize, R, W> IoCore<N, R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub async fn run(mut self, read_buffer_size: usize) -> Result<(), CoreError> {
        let mut bytes_until_next_parse = Header::SIZE;

        let notified = self.shared.wait_queue_updated.notified();

        loop {
            // Note: There is a world in which we find a way to reuse some of the futures instead
            //       of recreating them with every loop iteration, but I was not able to convince
            //       the borrow checker yet.

            tokio::select! {
                biased;  // We do not need the bias, but we want to avoid randomness overhead.

                // Writing outgoing data:
                write_result = self.writer.write_all_buf(self.current_frame.as_mut().unwrap())
                    , if self.current_frame.is_some() => {
                    write_result.map_err(CoreError::WriteFailed)?;

                    // We finished writing a frame, so prepare the next.
                    self.current_frame = self.ready_next_frame()?;
                }

                // Reading incoming data:
                read_result = read_atleast_bytesmut(&mut self.reader, &mut self.buffer, bytes_until_next_parse) => {
                    let bytes_read = read_result.map_err(CoreError::ReadFailed)?;

                    bytes_until_next_parse = bytes_until_next_parse.saturating_sub(bytes_read);

                    if bytes_until_next_parse == 0 {
                        match self.juliet.process_incoming(&mut self.buffer) {
                            Outcome::Incomplete(n) => {
                                // Simply reset how many bytes we need until the next parse.
                                bytes_until_next_parse = n.get() as usize;
                            },
                            Outcome::Fatal(err) => {
                                self.handle_fatal_read_err(err)
                            },
                            Outcome::Success(successful_read) => {
                                self.handle_completed_read(successful_read)
                            },
                        }
                    }

                    if bytes_read == 0 {
                        // Remote peer hung up.
                        return Ok(());
                    }
                }
            }
        }
    }

    fn handle_completed_read(&mut self, read: CompletedRead) {
        match read {
            CompletedRead::ErrorReceived { header, data } => todo!(),
            CompletedRead::NewRequest { id, payload } => todo!(),
            CompletedRead::ReceivedResponse { id, payload } => todo!(),
            CompletedRead::RequestCancellation { id } => todo!(),
            CompletedRead::ResponseCancellation { id } => todo!(),
        }
    }

    fn handle_fatal_read_err(&mut self, err: OutgoingMessage) {
        todo!()
    }

    /// Clears a potentially finished frame and returns the best next frame to send.
    ///
    /// Returns `None` if no frames are ready to be sent. Note that there may be frames waiting
    /// that cannot be sent due them being multi-frame messages when there already is a multi-frame
    /// message in progress, or request limits being hit.
    fn ready_next_frame(&mut self) -> Result<Option<OutgoingFrame>, LocalProtocolViolation> {
        // If we still have frame data, return it or take something from the ready queue.
        if let Some(current_frame) = self.current_frame.take() {
            if current_frame.has_remaining() {
                // Current frame is not done. This should usually not happen, but we can give a
                // correct answer regardless.
                return Ok(Some(current_frame));
            }
        }

        debug_assert!(self.current_frame.is_none()); // Guaranteed as this point.

        // Try to fetch a frame from the run queue. If there is nothing, we are stuck for now.
        let (frame, more) = match self.ready_queue.pop_front() {
            Some(item) => item,
            None => return Ok(None),
        }
        // Queue is empty, there is no next frame.
        .next_owned(self.juliet.max_frame_size());

        // If there are more frames after this one, schedule them again.
        if let Some(next_frame_iter) = more {
            self.ready_queue.push_back(next_frame_iter);
        } else {
            // No additional frames, check if we are about to finish a multi-frame transfer.
            let about_to_finish = frame.header();
            if let Some(ref active_multi) =
                self.active_multi_frame[about_to_finish.channel().get() as usize]
            {
                if about_to_finish == *active_multi {
                    // Once the scheduled frame is processed, we will finished the multi-frame
                    // transfer, so we can allow for the next multi-frame transfer to be scheduled.
                    self.active_multi_frame[about_to_finish.channel().get() as usize] = None;

                    // There is a chance another multi-frame messages became ready now.
                    self.process_wait_queue(about_to_finish.channel());
                }
            }
        }

        Ok(Some(frame))
    }

    /// Process the wait queue, moving messages that are ready to be sent to the ready queue.
    fn process_wait_queue(&mut self, channel: ChannelId) -> Result<(), LocalProtocolViolation> {
        let active_multi_frame = &self.active_multi_frame[channel.get() as usize];
        let mut wait_queue = self.shared.wait_queue[channel.get() as usize]
            .lock()
            .expect("lock poisoned");
        for _ in 0..(wait_queue.len()) {
            // Note: We do not use `drain` here, since we want to modify in-place. `retain` is also
            //       not used, since it does not allow taking out items by-value. An alternative
            //       might be sorting the list and splitting off the candidates instead.
            let item = wait_queue
                .pop_front()
                .expect("did not expect to run out of items");

            if item_is_ready(channel, &item, &self.juliet, active_multi_frame) {
                match item {
                    QueuedItem::Request { payload } => {
                        let msg = self.juliet.create_request(channel, payload)?;
                        self.ready_queue.push_back(msg.frames());
                    }
                    QueuedItem::Response { id, payload } => {
                        if let Some(msg) = self.juliet.create_response(channel, id, payload)? {
                            self.ready_queue.push_back(msg.frames());
                        }
                    }
                    QueuedItem::RequestCancellation { id } => {
                        if let Some(msg) = self.juliet.cancel_request(channel, id)? {
                            self.ready_queue.push_back(msg.frames());
                        }
                    }
                    QueuedItem::ResponseCancellation { id } => {
                        if let Some(msg) = self.juliet.cancel_response(channel, id)? {
                            self.ready_queue.push_back(msg.frames());
                        }
                    }
                    QueuedItem::Error { id, payload } => {
                        let msg = self.juliet.custom_error(channel, id, payload)?;
                        // Errors go into the front.
                        self.ready_queue.push_front(msg.frames());
                    }
                }
            } else {
                wait_queue.push_back(item);
            }
        }

        Ok(())
    }
}

fn item_is_ready<const N: usize>(
    channel: ChannelId,
    item: &QueuedItem,
    juliet: &JulietProtocol<N>,
    active_multi_frame: &Option<Header>,
) -> bool {
    // Check if we cannot schedule due to the message exceeding the request limit.
    if item.is_request() {
        if !juliet
            .allowed_to_send_request(channel)
            .expect("should not be called with invalid channel")
        {
            return false;
        }
    }

    // Check if we cannot schedule due to the message being multi-frame and there being a
    // multi-frame send in progress:
    if active_multi_frame.is_some() {
        if item.is_multi_frame(juliet.max_frame_size()) {
            return false;
        }
    }

    // Otherwise, this should be a legitimate add to the run queue.
    true
}

struct IoHandle<const N: usize> {
    shared: Arc<IoShared<N>>,
}

impl<const N: usize> IoHandle<N> {
    fn enqueue_request(
        &self,
        channel: ChannelId,
        payload: Option<Bytes>,
    ) -> Result<Option<Option<Bytes>>, LocalProtocolViolation> {
        bounds_check::<N>(channel)?;

        let count = &self.shared.requests_buffered[channel.get() as usize];
        let limit = self.shared.requests_limit[channel.get() as usize];

        // TODO: relax ordering from `SeqCst`.
        match count.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            if current < limit {
                Some(current + 1)
            } else {
                None
            }
        }) {
            Ok(_prev) => {
                // We successfully increment the count.
                let mut wait_queue = self.shared.wait_queue[channel.get() as usize]
                    .lock()
                    .expect("lock poisoned");
                wait_queue.push_back(QueuedItem::Request { payload });
                Ok(None)
            }
            Err(_prev) => Ok(Some(payload)),
        }
    }

    fn enqueue_response(
        &self,
        channel: ChannelId,
        id: Id,
        payload: Option<Bytes>,
    ) -> Result<(), LocalProtocolViolation> {
        bounds_check::<N>(channel)?;

        let mut wait_queue = self.shared.wait_queue[channel.get() as usize]
            .lock()
            .expect("lock poisoned");
        wait_queue.push_back(QueuedItem::Response { id, payload });

        Ok(())
    }

    fn enqueue_request_cancellation(
        &self,
        channel: ChannelId,
        id: Id,
    ) -> Result<(), LocalProtocolViolation> {
        bounds_check::<N>(channel)?;

        let mut wait_queue = self.shared.wait_queue[channel.get() as usize]
            .lock()
            .expect("lock poisoned");
        wait_queue.push_back(QueuedItem::RequestCancellation { id });
        Ok(())
    }

    fn enqueue_response_cancellation(
        &self,
        channel: ChannelId,
        id: Id,
    ) -> Result<(), LocalProtocolViolation> {
        bounds_check::<N>(channel)?;

        let mut wait_queue = self.shared.wait_queue[channel.get() as usize]
            .lock()
            .expect("lock poisoned");
        wait_queue.push_back(QueuedItem::ResponseCancellation { id });
        Ok(())
    }

    fn enqueue_error(
        &self,
        channel: ChannelId,
        id: Id,
        payload: Bytes,
    ) -> Result<(), LocalProtocolViolation> {
        let mut wait_queue = self.shared.wait_queue[channel.get() as usize]
            .lock()
            .expect("lock poisoned");
        wait_queue.push_back(QueuedItem::Error { id, payload });
        Ok(())
    }
}

/// Read bytes into a buffer.
///
/// Similar to [`AsyncReadExt::read_buf`], except it performs multiple read calls until at least
/// `target` bytes have been read.
///
/// Will automatically retry if an [`io::ErrorKind::Interrupted`] is returned.
///
/// # Cancellation safety
///
/// This function is cancellation safe in the same way that [`AsyncReadExt::read_buf`] is.
async fn read_atleast_bytesmut<'a, R>(
    reader: &'a mut R,
    buf: &mut BytesMut,
    target: usize,
) -> io::Result<usize>
where
    R: AsyncReadExt + Sized + Unpin,
{
    let mut bytes_read = 0;
    buf.reserve(target);

    while bytes_read < target {
        match reader.read_buf(buf).await {
            Ok(n) => bytes_read += n,
            Err(err) => {
                if matches!(err.kind(), io::ErrorKind::Interrupted) {
                    continue;
                }
                return Err(err);
            }
        }
    }

    Ok(bytes_read)
}

#[inline(always)]
fn bounds_check<const N: usize>(channel: ChannelId) -> Result<(), LocalProtocolViolation> {
    if channel.get() as usize >= N {
        Err(LocalProtocolViolation::InvalidChannel(channel))
    } else {
        Ok(())
    }
}
