use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use tokio::{sync::Notify, time::timeout_at};

use crate::transport::VideoFrame;

pub const MAX_DELAY_MS: u32 = 3_000;
const MAX_QUEUE_FRAMES: usize = 150;
const MAX_QUEUE_BYTES: usize = 12 * 1024 * 1024;
const LATE_LIMIT: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Queue {
    epoch: u32,
    delay_ms: u32,
    bytes: usize,
    needs_key: bool,
    frames: VecDeque<(Instant, VideoFrame)>,
}

impl Queue {
    fn pop(&mut self) -> Option<VideoFrame> {
        let (_, frame) = self.frames.pop_front()?;
        self.bytes -= frame.data.len();
        Some(frame)
    }

    fn skip_to_key(&mut self) {
        self.needs_key = true;
        while self.frames.front().is_some_and(|(_, frame)| !frame.key) {
            self.pop();
        }
    }
}

#[derive(Default)]
pub struct Playback {
    queue: Mutex<Queue>,
    changed: Notify,
}

impl Playback {
    pub fn set_delay(&self, delay_ms: u32) {
        self.queue.lock().unwrap().delay_ms = delay_ms.min(MAX_DELAY_MS);
        self.changed.notify_waiters();
    }

    pub fn delay(&self) -> u32 {
        self.queue.lock().unwrap().delay_ms
    }

    pub fn reset(&self) {
        let mut queue = self.queue.lock().unwrap();
        *queue = Queue {
            epoch: queue.epoch.wrapping_add(1),
            delay_ms: queue.delay_ms,
            needs_key: true,
            ..Queue::default()
        };
        self.changed.notify_waiters();
    }

    pub fn push(&self, frame: VideoFrame) {
        let mut queue = self.queue.lock().unwrap();
        queue.bytes += frame.data.len();
        queue.frames.push_back((Instant::now(), frame));
        let mut overflowed = false;
        while queue.frames.len() > MAX_QUEUE_FRAMES || queue.bytes > MAX_QUEUE_BYTES {
            queue.pop();
            overflowed = true;
        }
        if overflowed {
            queue.skip_to_key();
        }
        self.changed.notify_waiters();
    }

    // The viewer pulls one encoded frame at a time over binary Tauri IPC.
    // A reset has just the four-byte epoch. An empty payload is a heartbeat.
    pub async fn read(&self, viewer_epoch: u32) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_millis(600);
        loop {
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            let wake = {
                let mut queue = self.queue.lock().unwrap();
                let epoch = queue.epoch.to_be_bytes();
                if viewer_epoch != queue.epoch {
                    return epoch.to_vec();
                }
                let delay = Duration::from_millis(u64::from(queue.delay_ms));
                let now = Instant::now();
                while queue
                    .frames
                    .front()
                    .is_some_and(|(arrival, _)| *arrival + delay + LATE_LIMIT < now)
                {
                    queue.pop();
                    queue.needs_key = true;
                }
                if queue.needs_key {
                    queue.skip_to_key();
                }
                match queue.frames.front().map(|(arrival, _)| *arrival + delay) {
                    Some(due) if due <= now => {
                        let frame = queue.pop().expect("front frame");
                        queue.needs_key = false;
                        let mut packet = Vec::with_capacity(5 + frame.data.len());
                        packet.extend_from_slice(&epoch);
                        packet.push(u8::from(frame.key));
                        packet.extend_from_slice(&frame.data);
                        return packet;
                    }
                    Some(due) => due.min(deadline),
                    None => deadline,
                }
            };
            if timeout_at(wake.into(), changed).await.is_err() && wake == deadline {
                return Vec::new();
            }
        }
    }
}
