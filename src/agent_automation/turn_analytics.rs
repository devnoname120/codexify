//! Passive local estimates. No analytics result is used to drive automation.
use super::{AutomationActionStatus, AutomationTaskState, GoalStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
};

const FILE: &str = "turn-cutoff-estimates.json";
const MAX_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_DURATION_MS: u64 = 5 * 60 * 60 * 1000;
const MAX_SAMPLES: usize = 256;
const MAX_GAP_MS: u64 = 90_000;
const OFFLINE_MS: u64 = 240_000; // Statistical observation window, independent of presence settings.

#[derive(Default)]
struct ToolCounter {
    active: AtomicUsize,
    finished_at: AtomicU64,
}
static TOOLS: OnceLock<Mutex<HashMap<String, Arc<ToolCounter>>>> = OnceLock::new();
static UNTRACKED: ToolCounter = ToolCounter {
    active: AtomicUsize::new(0),
    finished_at: AtomicU64::new(0),
};
pub(crate) struct ToolGuard {
    counter: Option<Arc<ToolCounter>>,
}
impl Drop for ToolGuard {
    fn drop(&mut self) {
        let c = self.counter.as_deref().unwrap_or(&UNTRACKED);
        c.finished_at
            .fetch_max(crate::markdown_chat::now_ms(), Ordering::Relaxed);
        c.active.fetch_sub(1, Ordering::Release);
    }
}
fn key(task: &str, owner: u64) -> String {
    format!("{task}:{owner}")
}
pub(crate) fn begin_tool(task: &str, owner: u64) -> ToolGuard {
    let counter = TOOLS
        .get_or_init(Default::default)
        .try_lock()
        .ok()
        .and_then(|mut map| {
            if map.len() >= 4096 {
                let since = crate::markdown_chat::now_ms().saturating_sub(24 * 60 * 60 * 1000);
                map.retain(|_, c| {
                    c.active.load(Ordering::Acquire) > 0
                        || c.finished_at.load(Ordering::Relaxed) >= since
                });
            }
            let k = key(task, owner);
            if map.len() >= 4096 && !map.contains_key(&k) {
                return None;
            }
            let c = map.entry(k).or_default().clone();
            c.active.fetch_add(1, Ordering::AcqRel);
            Some(c)
        });
    if counter.is_none() {
        UNTRACKED.active.fetch_add(1, Ordering::AcqRel);
    }
    ToolGuard { counter }
}
#[derive(Clone, Default)]
struct NativeActivity {
    active: usize,
    finished_at: u64,
    reliable: bool,
}
fn native_activity(task: &str, owner: u64) -> NativeActivity {
    if UNTRACKED.active.load(Ordering::Acquire) > 0 {
        return NativeActivity::default();
    }
    let Some(Ok(map)) = TOOLS.get().map(|m| m.try_lock()) else {
        return NativeActivity {
            reliable: TOOLS.get().is_none(),
            ..Default::default()
        };
    };
    let c = map.get(&key(task, owner));
    NativeActivity {
        active: c.map_or(0, |c| c.active.load(Ordering::Acquire)),
        finished_at: c
            .map_or(0, |c| c.finished_at.load(Ordering::Relaxed))
            .max(UNTRACKED.finished_at.load(Ordering::Relaxed)),
        reliable: true,
    }
}

#[derive(Clone)]
struct Observation {
    chat: PathBuf,
    id: String,
    model: String,
    effort: String,
    start: u64,
    last_tool: Option<u64>,
    now: u64,
    renewal: bool,
    stopped: bool,
    unfinished: bool,
    matched: bool,
    native: NativeActivity,
}
struct Sink {
    tx: SyncSender<Observation>,
}
static SINK: OnceLock<Option<Sink>> = OnceLock::new();
pub(crate) fn initialize() {
    SINK.get_or_init(|| {
        let (tx, rx) = sync_channel::<Observation>(256);
        let started = crate::markdown_chat::now_ms();
        let epoch = format!("{}-{started}", std::process::id());
        std::thread::Builder::new()
            .name("turn-cutoff-estimates".into())
            .spawn(move || {
                for observation in rx {
                    // Analytics has its own thread, files and bounded input queue. No
                    // caller waits for storage, and errors cannot enter action state.
                    if let Err(error) = collect(&observation, &epoch, started) {
                        tracing::debug!(%error,"cutoff observation skipped");
                    }
                }
            })
            .ok()
            .map(|_| Sink { tx })
    });
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn observe(
    chat: &Path,
    task: &str,
    owner: u64,
    state: &AutomationTaskState,
    last_tool: Option<u64>,
    stopped: bool,
    renewal: bool,
    now: u64,
) {
    let Some(observation) = snapshot(chat, task, owner, state, last_tool, stopped, renewal, now)
    else {
        return;
    };
    initialize();
    if let Some(sink) = SINK.get().and_then(Option::as_ref) {
        let _ = sink.tx.try_send(observation);
    }
}
#[allow(clippy::too_many_arguments)]
fn snapshot(
    chat: &Path,
    task: &str,
    owner: u64,
    state: &AutomationTaskState,
    last_tool: Option<u64>,
    stopped: bool,
    renewal: bool,
    now: u64,
) -> Option<Observation> {
    let send = state.turn_renewal.send.as_ref()?;
    let (Some(model), Some(effort)) = (&send.model, &send.effort) else {
        return None;
    };
    let valid_pair = send.owner_generation == owner
        && send.pairing_generation == state.pairing.generation
        && state.pairing.verified_owner_generation == Some(owner);
    let unfinished = state
        .goal
        .as_ref()
        .is_none_or(|g| g.status == GoalStatus::Active);
    let id = format!(
        "{:x}",
        Sha256::digest(format!(
            "{}\0{task}\0{owner}\0{}\0{}",
            chat.display(),
            send.pairing_generation,
            send.action_id
        ))
    );
    Some(Observation {
        chat: chat.into(),
        id,
        model: model.clone(),
        effort: effort.clone(),
        start: send.at_ms,
        last_tool,
        now,
        renewal: renewal || state.turn_renewal.handoff.is_some(),
        stopped: stopped || !valid_pair || state.action.status == AutomationActionStatus::Cancelled,
        unfinished,
        matched: state.action.id.as_deref() == Some(send.action_id.as_str()),
        native: native_activity(task, owner),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Sample {
    id: String,
    model: String,
    effort: String,
    send_at_ms: u64,
    last_tool_at_ms: u64,
    detected_at_ms: u64,
    duration_ms: u64,
}
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct Current {
    id: String,
    model: String,
    effort: String,
    start: u64,
    last_tool: Option<u64>,
    epoch: String,
    last_observation: u64,
    observed_active: bool,
    renewal_seen: bool,
    stopped: bool,
    sample: Option<Sample>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Data {
    version: u32,
    current: Option<Current>,
    samples: Vec<Sample>,
}
impl Default for Data {
    fn default() -> Self {
        Self {
            version: 1,
            current: None,
            samples: Vec::new(),
        }
    }
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct QueueIntent {
    completed_waiting: bool,
    input_waiting: bool,
}
fn update(data: &mut Data, o: &Observation, epoch: &str, started: u64, intent: &QueueIntent) {
    if data.current.as_ref().is_some_and(|c| c.id != o.id) {
        if let Some(sample) = data.current.take().and_then(|c| c.sample) {
            data.samples.retain(|s| s.id != sample.id);
            data.samples.push(sample);
        }
        let excess = data.samples.len().saturating_sub(MAX_SAMPLES);
        data.samples.drain(..excess);
    }
    let c = data.current.get_or_insert_with(|| Current {
        id: o.id.clone(),
        model: o.model.clone(),
        effort: o.effort.clone(),
        start: o.start,
        ..Default::default()
    });
    if data.samples.iter().any(|s| s.id == c.id) {
        return;
    }
    let new_epoch = c.epoch != epoch;
    let backwards = o.now < c.last_observation;
    let gap = c.last_observation > 0 && o.now.saturating_sub(c.last_observation) > MAX_GAP_MS;
    if new_epoch || gap {
        c.observed_active = false;
    }
    c.epoch = epoch.into();
    c.last_observation = o.now;
    c.start = c.start.min(o.start);
    c.model = o.model.clone();
    c.effort = o.effort.clone();
    c.renewal_seen |= o.renewal;
    c.stopped |= o.stopped;
    if o.last_tool != c.last_tool {
        c.sample = None;
        c.last_tool = o.last_tool;
    }
    let eligible = o.unfinished
        && !intent.completed_waiting
        && !intent.input_waiting
        && o.native.reliable
        && !c.renewal_seen
        && !c.stopped;
    if !eligible || backwards {
        c.sample = None;
        c.observed_active = false;
        return;
    }
    let Some(last) = c.last_tool.filter(|at| *at >= c.start && *at <= o.now) else {
        c.sample = None;
        return;
    };
    if o.native.finished_at > o.now {
        c.sample = None;
        c.observed_active = false;
        return;
    }
    // A new action can be queued before its send is confirmed. Preserve an
    // existing estimate until that send seals it, but never infer a new one
    // from activity that may belong to the next action.
    if !o.matched {
        if o.native.active > 0 {
            c.sample = None;
        }
        c.observed_active = false;
        return;
    }
    let quiet_since = last.max(o.native.finished_at);
    let running =
        o.native.active > 0 || (last >= started && o.now.saturating_sub(quiet_since) < OFFLINE_MS);
    if running {
        c.observed_active = true;
        c.sample = None;
        return;
    }
    if !c.observed_active {
        return;
    }
    let duration = last.saturating_sub(c.start);
    if !(1..=MAX_DURATION_MS).contains(&duration) {
        c.sample = None;
        return;
    }
    c.sample = Some(Sample {
        id: c.id.clone(),
        model: c.model.clone(),
        effort: c.effort.clone(),
        send_at_ms: c.start,
        last_tool_at_ms: last,
        detected_at_ms: c.sample.as_ref().map_or(o.now, |s| s.detected_at_ms),
        duration_ms: duration,
    });
}

fn read(path: &Path, max: u64) -> Result<Option<Vec<u8>>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("analytics metadata unavailable".into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max {
        return Err("analytics record not regular or oversized".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|_| "analytics read unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "analytics metadata unavailable")?
        .is_file()
    {
        return Err("analytics record changed".into());
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "analytics read unavailable")?;
    if bytes.len() as u64 > max {
        return Err("analytics record oversized".into());
    }
    Ok(Some(bytes))
}
fn load(path: &Path) -> Result<Data, String> {
    let Some(bytes) = read(path, MAX_BYTES)? else {
        return Ok(Data::default());
    };
    let data: Data = serde_json::from_slice(&bytes).map_err(|_| "analytics record invalid")?;
    if data.version != 1 || data.samples.len() > MAX_SAMPLES {
        return Err("analytics record unsupported".into());
    }
    Ok(data)
}
fn save(path: &Path, data: &Data) -> Result<(), String> {
    let bytes = serde_json::to_vec(data).map_err(|_| "analytics encoding failed")?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("analytics record oversized".into());
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err("analytics path is not regular".into());
    }
    let parent = path.parent().ok_or("analytics directory unavailable")?;
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| "analytics write unavailable")?;
    file.write_all(&bytes)
        .map_err(|_| "analytics write unavailable")?;
    // Deliberately no fsync: losing a best-effort estimate must not add disk
    // flush latency to the rest of the application. persist replaces atomically.
    file.persist(path)
        .map_err(|_| "analytics publish unavailable")?;
    Ok(())
}
fn collect(o: &Observation, epoch: &str, started: u64) -> Result<(), String> {
    let path = o.chat.with_file_name(FILE);
    let mut data = load(&path)?;
    let queue = o.chat.with_file_name("message-queue.json");
    let intent: QueueIntent = match read(&queue, 16 * 1024 * 1024)? {
        Some(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| "analytics queue intent unavailable")?
        }
        None => QueueIntent::default(),
    };
    let before = data.clone();
    update(&mut data, o, epoch, started, &intent);
    if data != before {
        save(&path, &data)?;
    }
    Ok(())
}

#[derive(Default, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub models: Vec<ModelEstimate>,
    pub samples: usize,
}
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelEstimate {
    model: String,
    effort: String,
    samples: usize,
    median_ms: u64,
    p90_ms: u64,
    min_ms: u64,
    max_ms: u64,
}
pub(crate) fn summary(chats: impl IntoIterator<Item = PathBuf>) -> Summary {
    let mut groups: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for chat in chats.into_iter().take(4096) {
        let Ok(data) = load(&chat.with_file_name(FILE)) else {
            continue;
        };
        for s in data
            .samples
            .into_iter()
            .chain(data.current.and_then(|c| c.sample))
        {
            if !(1..=MAX_DURATION_MS).contains(&s.duration_ms)
                || s.last_tool_at_ms.checked_sub(s.send_at_ms) != Some(s.duration_ms)
                || s.send_at_ms == 0
                || s.detected_at_ms > crate::markdown_chat::now_ms()
                || s.detected_at_ms < s.last_tool_at_ms
                || s.model.is_empty()
                || s.model.len() > 256
                || s.effort.is_empty()
                || s.effort.len() > 256
                || !seen.insert(s.id)
            {
                continue;
            }
            groups
                .entry((s.model, s.effort))
                .or_default()
                .push(s.duration_ms);
        }
    }
    let mut output = Summary::default();
    for ((model, effort), mut values) in groups {
        values.sort_unstable();
        let n = values.len();
        let median_ms = if n % 2 == 0 {
            (values[n / 2 - 1] + values[n / 2]) / 2
        } else {
            values[n / 2]
        };
        output.samples += n;
        output.models.push(ModelEstimate {
            model,
            effort,
            samples: n,
            median_ms,
            p90_ms: values[(n * 9).div_ceil(10) - 1],
            min_ms: values[0],
            max_ms: values[n - 1],
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    const START: u64 = 1_000_000;
    fn observation() -> Observation {
        Observation {
            chat: PathBuf::from("/unused/CHAT.md"),
            id: "first".into(),
            model: "gpt-5.6-sol".into(),
            effort: "max".into(),
            start: START,
            last_tool: Some(START + 60_000),
            now: START + 65_000,
            renewal: false,
            stopped: false,
            unfinished: true,
            matched: true,
            native: NativeActivity {
                reliable: true,
                finished_at: START + 61_000,
                ..Default::default()
            },
        }
    }
    fn tick(data: &mut Data, o: &Observation) {
        update(data, o, "process-one", START, &QueueIntent::default());
    }
    fn quiet(data: &mut Data, o: &mut Observation) {
        for _ in 0..6 {
            tick(data, o);
            o.now += 60_000;
        }
    }
    fn sample(data: &Data) -> Option<&Sample> {
        data.current.as_ref().and_then(|c| c.sample.as_ref())
    }
    #[test]
    fn only_confirmed_sends_use_actual_model_and_effort_not_saved_preference() {
        use crate::agent_automation::turn_renewal::TurnSend;
        let chat = Path::new("/unused/CHAT.md");
        let mut state = AutomationTaskState::default();
        assert!(snapshot(chat, "chat", 3, &state, None, false, false, START).is_none());
        state.selection.model = Some("preferred-model".into());
        state.selection.effort = Some("max".into());
        state.pairing.generation = 4;
        state.pairing.verified_owner_generation = Some(3);
        state.action.id = Some("accepted".into());
        state.turn_renewal.send = Some(TurnSend {
            action_id: "accepted".into(),
            at_ms: START,
            model: Some("actual-fallback-model".into()),
            effort: Some("high".into()),
            pairing_generation: 4,
            owner_generation: 3,
        });
        let o = snapshot(
            chat,
            "chat",
            3,
            &state,
            Some(START + 1),
            false,
            false,
            START + 2,
        )
        .unwrap();
        assert_eq!(
            (o.model.as_str(), o.effort.as_str(), o.start),
            ("actual-fallback-model", "high", START)
        );
        assert!(o.matched && o.unfinished && !o.stopped);
        assert!(
            snapshot(chat, "chat", 5, &state, None, false, false, START)
                .unwrap()
                .stopped
        );
        state.pairing.generation = 5;
        assert!(
            snapshot(chat, "chat", 3, &state, None, false, false, START)
                .unwrap()
                .stopped
        );
        state.turn_renewal.send.as_mut().unwrap().model = None;
        assert!(snapshot(chat, "chat", 3, &state, None, false, false, START).is_none());
    }
    #[test]
    fn measures_accepted_send_to_last_call_without_offline_delay_and_deduplicates() {
        let mut data = Data::default();
        let mut o = observation();
        quiet(&mut data, &mut o);
        let s = sample(&data).unwrap();
        assert_eq!(s.duration_ms, 60_000);
        assert!(s.detected_at_ms - s.last_tool_at_ms >= OFFLINE_MS);
        assert_eq!((&s.model, &s.effort), (&o.model, &o.effort));
        let first = s.clone();
        tick(&mut data, &o);
        assert_eq!(sample(&data), Some(&first));
        assert!(data.samples.is_empty());
    }
    #[test]
    fn active_native_tool_and_completion_grace_do_not_count_as_expiration() {
        let mut data = Data::default();
        let mut o = observation();
        o.native.active = 1;
        quiet(&mut data, &mut o);
        assert!(sample(&data).is_none());
        o.native.active = 0;
        o.native.finished_at = o.now;
        tick(&mut data, &o);
        assert!(sample(&data).is_none());
        quiet(&mut data, &mut o);
        assert_eq!(sample(&data).unwrap().duration_ms, 60_000);
    }
    #[test]
    fn renewed_stopped_completed_and_input_turns_never_produce_estimates() {
        for kind in 0..7 {
            let mut data = Data::default();
            let mut o = observation();
            tick(&mut data, &o);
            match kind {
                0 => o.renewal = true,
                1 => o.stopped = true,
                2 => o.unfinished = false,
                3 => o.matched = false,
                4 => o.native.reliable = false,
                _ => {}
            }
            let intent = QueueIntent {
                completed_waiting: kind == 5,
                input_waiting: kind == 6,
            };
            for _ in 0..7 {
                update(&mut data, &o, "process-one", START, &intent);
                o.now += 60_000;
            }
            assert!(sample(&data).is_none(), "exclusion {kind}");
        }
    }
    #[test]
    fn enabling_renewal_or_stopping_once_excludes_the_entire_current_send() {
        for stop in [true, false] {
            let mut data = Data::default();
            let mut o = observation();
            o.stopped = stop;
            o.renewal = !stop;
            tick(&mut data, &o);
            o.stopped = false;
            o.renewal = false;
            quiet(&mut data, &mut o);
            assert!(sample(&data).is_none());
        }
    }
    #[test]
    fn resumed_activity_and_late_completion_withdraw_the_current_estimate() {
        for input in [false, true] {
            let mut data = Data::default();
            let mut o = observation();
            quiet(&mut data, &mut o);
            assert!(sample(&data).is_some());
            if input {
                update(
                    &mut data,
                    &o,
                    "process-one",
                    START,
                    &QueueIntent {
                        input_waiting: true,
                        ..Default::default()
                    },
                );
            } else {
                o.last_tool = Some(o.now);
                o.native.finished_at = o.now;
                tick(&mut data, &o);
            }
            assert!(sample(&data).is_none());
        }
    }
    #[test]
    fn next_pending_action_preserves_then_next_confirmed_send_seals_previous_estimate() {
        let mut data = Data::default();
        let mut o = observation();
        quiet(&mut data, &mut o);
        o.matched = false;
        tick(&mut data, &o);
        assert!(sample(&data).is_some());
        o.id = "second".into();
        o.start = o.now;
        o.last_tool = None;
        o.matched = true;
        tick(&mut data, &o);
        assert_eq!(data.samples.len(), 1);
        assert_eq!(data.samples[0].id, "first");
        assert!(sample(&data).is_none());
        tick(&mut data, &o);
        assert_eq!(data.samples.len(), 1);
    }
    #[test]
    fn missed_scans_and_restart_require_new_activity_before_new_estimates() {
        let mut data = Data::default();
        let mut o = observation();
        tick(&mut data, &o);
        o.now += MAX_GAP_MS + OFFLINE_MS;
        tick(&mut data, &o);
        assert!(sample(&data).is_none());
        update(&mut data, &o, "process-two", o.now, &QueueIntent::default());
        assert!(sample(&data).is_none());
        o.last_tool = Some(o.now);
        o.native.finished_at = o.now;
        quiet(&mut data, &mut o);
        assert!(sample(&data).is_some());
    }
    #[test]
    fn invalid_durations_and_clock_anomalies_are_rejected_but_five_hours_is_allowed() {
        for duration in [0, MAX_DURATION_MS, MAX_DURATION_MS + 1] {
            let mut data = Data::default();
            let mut o = observation();
            o.last_tool = Some(START + duration);
            o.now = START + duration + 1;
            o.native.finished_at = START + duration;
            quiet(&mut data, &mut o);
            assert_eq!(sample(&data).is_some(), duration == MAX_DURATION_MS);
        }
        for (last, finished) in [
            (START - 1, START),
            (START + 100_000_000, START),
            (START + 60_000, START + 100_000_000),
        ] {
            let mut data = Data::default();
            let mut o = observation();
            o.last_tool = Some(last);
            o.native.finished_at = finished;
            quiet(&mut data, &mut o);
            assert!(sample(&data).is_none());
        }
    }
    #[test]
    fn storage_summary_buckets_models_and_efforts_and_filters_corrupt_samples() {
        let dir = tempfile::tempdir().unwrap();
        let chat = dir.path().join("CHAT.md");
        let mut data = Data::default();
        let mut o = observation();
        quiet(&mut data, &mut o);
        let valid = sample(&data).unwrap().clone();
        data.current = None;
        for (index, duration) in [60_000, 120_000, 180_000, 240_000].into_iter().enumerate() {
            let mut s = valid.clone();
            s.id = format!("sample-{index}");
            s.duration_ms = duration;
            s.last_tool_at_ms = s.send_at_ms + duration;
            s.detected_at_ms = s.last_tool_at_ms + OFFLINE_MS;
            data.samples.push(s);
        }
        let mut other = valid.clone();
        other.id = "other-effort".into();
        other.effort = "high".into();
        data.samples.push(other);
        let mut other = valid.clone();
        other.id = "other-model".into();
        other.model = "gpt-6-astra".into();
        data.samples.push(other);
        for duration in [0, MAX_DURATION_MS + 1] {
            let mut bad = valid.clone();
            bad.id = format!("bad-{duration}");
            bad.duration_ms = duration;
            data.samples.push(bad);
        }
        data.samples.push(valid.clone());
        data.samples.push(valid);
        save(&chat.with_file_name(FILE), &data).unwrap();
        let result = summary([chat.clone(), chat]);
        assert_eq!(result.samples, 7);
        assert_eq!(result.models.len(), 3);
        let model = result
            .models
            .iter()
            .find(|m| m.model == "gpt-5.6-sol" && m.effort == "max")
            .unwrap();
        assert_eq!(
            (
                model.samples,
                model.median_ms,
                model.p90_ms,
                model.min_ms,
                model.max_ms
            ),
            (5, 120_000, 240_000, 60_000, 240_000)
        );
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .all(|e| e.unwrap().file_name() == FILE)
        );
    }
    #[test]
    fn malformed_unsupported_oversized_and_nonregular_sidecars_are_ignored_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let chat = dir.path().join("CHAT.md");
        let path = chat.with_file_name(FILE);
        let mut o = observation();
        o.chat = chat.clone();
        let protected = dir.path().join("automation.json");
        std::fs::write(&protected, b"protected automation").unwrap();
        for bytes in [
            b"invalid JSON".to_vec(),
            br#"{"version":999,"current":null,"samples":[]}"#.to_vec(),
            vec![b'x'; MAX_BYTES as usize + 1],
        ] {
            std::fs::write(&path, &bytes).unwrap();
            assert!(collect(&o, "test", START).is_err());
            assert_eq!(summary([chat.clone()]).samples, 0);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(collect(&o, "test", START).is_err());
        assert_eq!(summary([chat]).samples, 0);
        assert_eq!(std::fs::read(&protected).unwrap(), b"protected automation");
    }
    #[cfg(unix)]
    #[test]
    fn symlink_records_are_not_followed_or_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let chat = dir.path().join("CHAT.md");
        let target = dir.path().join("protected");
        std::fs::write(&target, b"protected").unwrap();
        let path = chat.with_file_name(FILE);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(load(&path).is_err());
        assert!(save(&path, &Data::default()).is_err());
        assert_eq!(summary([chat]).samples, 0);
        assert_eq!(std::fs::read(target).unwrap(), b"protected");
    }
    #[test]
    fn corrupt_queue_intent_disables_collection_without_touching_delivery_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = observation();
        o.chat = dir.path().join("CHAT.md");
        let path = o.chat.with_file_name("message-queue.json");
        std::fs::write(&path, b"invalid").unwrap();
        assert!(collect(&o, "test", START).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid");
        assert!(!o.chat.with_file_name(FILE).exists());
    }
    #[test]
    fn native_tool_guard_tracks_scope_including_early_return_and_cancellation() {
        let task = "analytics-scope-test";
        let owner = 471;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let guard = loop {
            let guard = begin_tool(task, owner);
            let state = native_activity(task, owner);
            if state.reliable {
                assert_eq!(state.active, 1);
                break guard;
            }
            // Other parallel tests can briefly occupy the best-effort registry.
            // An untracked guard must be dropped before taking a fresh sample.
            drop(guard);
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        drop(guard);
        let state = loop {
            let state = native_activity(task, owner);
            if state.reliable {
                break state;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(state.active, 0);
        assert!(state.finished_at > 0);
    }
    #[test]
    fn historical_retention_is_bounded() {
        let mut data = Data::default();
        let mut o = observation();
        quiet(&mut data, &mut o);
        let original = sample(&data).unwrap().clone();
        for index in 0..MAX_SAMPLES + 10 {
            let mut s = original.clone();
            s.id = index.to_string();
            data.samples.push(s);
        }
        o.id = "next".into();
        tick(&mut data, &o);
        assert_eq!(data.samples.len(), MAX_SAMPLES);
        assert_eq!(data.samples.last().unwrap().id, "first");
    }
}
