//! Retained audio and the single Wispr processing slot. Capture and delivery
//! belong to the logical job, not to Wispr's later recording UUID.
use crate::{
    protocol::{Event, Timing},
    recorder,
};
use base64::Engine;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

// Wispr's paste can arrive after its idle status. Keep that route, but do
// not leave a broker worker alive forever for an empty/no-speech result.
const LATE_PASTE_FOR: Duration = Duration::from_secs(600);

pub struct Job<T> {
    pub destination: Option<T>,
    pub audio: Vec<u8>,
    pub timing: Option<Timing>,
    pub uuid: Option<String>,
    sent: usize,
    index: u64,
    pub released: bool,
    pub stopped: bool,
    pub claimed: bool,
    pub local_running: bool,
    pub cloud_done: bool,
    pub retrying: bool,
    pub external: bool,
    pub cancelled: bool,
    completed_at: Option<Instant>,
    pub icon: Option<std::sync::Arc<crate::icon::Icon>>,
}

pub struct Queue<T> {
    pub jobs: BTreeMap<u64, Job<T>>,
    waiting: VecDeque<u64>,
    pub active: Option<u64>,
    retired: HashSet<String>,
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self {
            jobs: BTreeMap::new(),
            waiting: VecDeque::new(),
            active: None,
            retired: HashSet::new(),
        }
    }
}

impl<T> Queue<T> {
    pub fn add(&mut self, id: u64, destination: T) {
        self.jobs.insert(
            id,
            Job {
                destination: Some(destination),
                audio: Vec::new(),
                timing: None,
                uuid: None,
                sent: 0,
                index: 0,
                released: false,
                stopped: false,
                claimed: false,
                local_running: false,
                cloud_done: false,
                retrying: false,
                external: false,
                cancelled: false,
                completed_at: None,
                icon: None,
            },
        );
        self.waiting.push_back(id);
    }

    pub fn next(&mut self) -> Option<u64> {
        if self.active.is_some() {
            return None;
        }
        while let Some(id) = self.waiting.pop_front() {
            if self.jobs.contains_key(&id) {
                self.active = Some(id);
                return Some(id);
            }
        }
        None
    }

    pub fn external(&mut self, id: u64, uuid: String) {
        self.waiting.retain(|queued| *queued != id);
        self.active = Some(id);
        let job = self.jobs.get_mut(&id).unwrap();
        job.uuid = Some(uuid);
        job.external = true;
    }

    pub fn bind(&mut self, uuid: String) -> Option<u64> {
        let id = self.active?;
        if self.jobs.get(&id)?.external || self.jobs.get(&id)?.uuid.is_some() {
            return None;
        }
        self.jobs.get_mut(&id)?.uuid = Some(uuid);
        Some(id)
    }

    pub fn id_for(&self, uuid: &str) -> Option<u64> {
        self.jobs
            .iter()
            .find_map(|(&id, job)| (job.uuid.as_deref() == Some(uuid)).then_some(id))
    }

    pub fn handled(&self, uuid: &str) -> bool {
        self.retired.contains(uuid) || self.id_for(uuid).is_some_and(|id| self.jobs[&id].claimed)
    }

    pub fn feed(&mut self, id: u64, pcm: &[u8]) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.audio.extend_from_slice(pcm);
        }
    }

    pub fn end(&mut self, id: u64, timing: Timing) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.timing = Some(timing);
        }
    }

    pub fn events(&mut self) -> Vec<Event> {
        let Some(job) = self.active.and_then(|id| self.jobs.get_mut(&id)) else {
            return vec![];
        };
        let Some(uuid) = job.uuid.clone() else {
            return vec![];
        };
        if job.external {
            return vec![];
        }
        let mut events = Vec::new();
        let available = job.audio.len() - job.sent;
        let bytes = if job.timing.is_some() {
            available & !1
        } else {
            available / recorder::CHUNK_BYTES * recorder::CHUNK_BYTES
        };
        for pcm in job.audio[job.sent..job.sent + bytes].chunks(recorder::CHUNK_BYTES) {
            events.push(Event::Chunk {
                uuid: uuid.clone(),
                index: job.index,
                volume: recorder::volume_db(pcm),
                duration: recorder::duration_ms(pcm.len()),
                pcm: base64::engine::general_purpose::STANDARD.encode(pcm),
            });
            job.index += 1;
        }
        job.sent += bytes;
        if job.timing.is_some() && !job.released {
            job.released = true;
            events.push(Event::WisprKey {
                action: "ptt".into(),
                down: false,
            });
        }
        events
    }

    pub fn stop(&mut self) -> Option<Event> {
        let job = self.jobs.get_mut(&self.active?)?;
        if job.stopped {
            return None;
        }
        let timing = job.timing.clone()?;
        let uuid = job.uuid.clone()?;
        job.stopped = true;
        Some(Event::Done { uuid, timing })
    }

    /// Both recognizers compete for one delivery. The loser never types a
    /// second result into an ordinary input or completes a broker twice.
    pub fn claim(&mut self, id: u64) -> Option<T> {
        let job = self.jobs.get_mut(&id)?;
        if job.claimed {
            return None;
        }
        job.claimed = true;
        job.destination.take()
    }

    pub fn available(&mut self) {
        if let Some(id) = self.active.take() {
            if let Some(job) = self.jobs.get_mut(&id) {
                job.cloud_done = true;
                job.completed_at = Some(Instant::now());
                // Idle releases Wispr's slot, but its final paste may still
                // be pending. Keep audio and the local alternative until a
                // transcript has actually claimed this job.
                if job.claimed {
                    job.audio.clear();
                    job.audio.shrink_to_fit();
                }
            }
        }
        self.prune();
    }

    pub fn prune(&mut self) {
        self.jobs.retain(|_, j| {
            let expired = j
                .completed_at
                .is_some_and(|at| at.elapsed() >= LATE_PASTE_FOR);
            let remove = j.cloud_done && (j.claimed || expired) && !j.local_running;
            if remove {
                if let Some(uuid) = &j.uuid {
                    self.retired.insert(uuid.clone());
                }
            }
            !remove
        });
    }

    pub fn cancel(&mut self, id: u64) {
        if self.active == Some(id) {
            if let Some(job) = self.jobs.get_mut(&id) {
                job.claimed = true;
                job.cancelled = true;
                job.destination = None;
                job.audio.clear();
                job.sent = 0;
            }
        } else {
            self.jobs.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn timing() -> Timing {
        Timing {
            start_time: 0.,
            stop_time: 100.,
            absolute_stop_time: 100.,
            audio_device_name: "mic".into(),
            mic_acquisition_succeeded: true,
            media_stream_setup: 0.,
            worklet_setup: 0.,
            first_audio_chunk_latency: 0.,
            recording_time: 100.,
            time_to_receive_last_audio_chunk: 0.,
            recording_finish: 0.,
        }
    }
    #[test]
    fn queued_audio_replays_once_in_order_and_keeps_the_original_destination() {
        let mut queue = Queue::default();
        queue.add(1, "first app");
        queue.add(2, "second app");
        queue.feed(1, &[1; 1280]);
        queue.feed(2, &[2; 2000]);
        queue.end(2, timing());
        assert_eq!(queue.next(), Some(1));
        assert_eq!(queue.next(), None);
        queue.bind("wispr-1".into());
        assert_eq!(queue.events().len(), 1);
        assert!(queue.events().is_empty());
        queue.end(1, timing());
        assert!(matches!(
            queue.events().as_slice(),
            [Event::WisprKey { down: false, .. }]
        ));
        assert!(matches!(queue.stop(), Some(Event::Done { .. })));
        assert!(queue.stop().is_none());
        assert_eq!(queue.claim(1), Some("first app"));
        queue.available();
        assert_eq!(queue.next(), Some(2));
        queue.bind("wispr-2".into());
        let events = queue.events();
        assert!(matches!(&events[0], Event::Chunk { index: 0, .. }));
        assert!(matches!(&events[1], Event::Chunk { index: 1, .. }));
        assert!(matches!(&events[2], Event::WisprKey { down: false, .. }));
        assert_eq!(queue.claim(2), Some("second app"));
        assert_eq!(queue.claim(2), None);
        assert_eq!(queue.jobs[&2].audio.len(), 2000);
    }
    #[test]
    fn local_and_cloud_race_for_a_single_delivery_without_cancelling_cloud() {
        let mut queue = Queue::default();
        queue.add(1, "captured");
        queue.next();
        queue.jobs.get_mut(&1).unwrap().local_running = true;
        assert_eq!(queue.claim(1), Some("captured"));
        assert_eq!(queue.active, Some(1));
        assert_eq!(queue.claim(1), None);
        queue.available();
        assert!(queue.jobs.contains_key(&1));
        queue.jobs.get_mut(&1).unwrap().local_running = false;
        queue.prune();
        assert!(queue.jobs.is_empty());
    }

    #[test]
    fn idle_without_a_transcript_keeps_audio_and_eventually_cancels_an_unused_route() {
        let mut queue = Queue::default();
        queue.add(1, "original app");
        queue.next();
        queue.bind("empty-result".into());
        queue.feed(1, &[1; 1280]);
        queue.available();
        assert_eq!(queue.jobs[&1].audio, vec![1; 1280]);
        assert_eq!(queue.jobs[&1].destination, Some("original app"));
        queue.jobs.get_mut(&1).unwrap().completed_at = Some(Instant::now() - LATE_PASTE_FOR);
        queue.prune();
        assert!(queue.jobs.is_empty());
        assert!(queue.handled("empty-result"));
    }
}
