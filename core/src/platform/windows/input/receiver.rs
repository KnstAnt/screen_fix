use std::{collections::BTreeMap, sync::mpsc::SyncSender};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{HKL, MAPVK_VK_TO_CHAR, MAPVK_VK_TO_VSC_EX, MapVirtualKeyExW};

use crate::{
    capture::{
        context::ControllerHint,
        input::{
            ControlChords, Discarded, HeldBoundary, HeldInput, InputGap,
            InputMatcher, InputStep, Key, MotionSampler,
            NormalizedInput, PendingInput, PhysicalHeld, ProvisionalInputChange, Sampled, ScanCode, ScanPrefix,
            modifier_bit, modifier_of, startup_modifier,
        },
        observation::{ProvisionalInputKind, ProvisionalInputObservation},
    },
    package::{
        diagnostics::{Failure, FailureComponent},
        journal::JournalIngress, metadata::{InputBlindReason, UncertifiedInputTail},
        outcome::{HealthKind, LossCount, LossEvidence, LossProducer, LossRange, LossReason, LossScope, SuppressionClass},
    },
    protocol::{
        AdmissionEpoch, BoundedList, DeviceId, Evidence, EventRef, FocusGeneration, GeometryId, MonitorId, ObservationId, PhysicalPoint, QpcTick, SessionTimeUs, UnavailableReason,
        ipc::{DeviceDescriptor, ForegroundAtReceipt, GapCause, HelperToCore, InputSample, MAX_INPUT_DEVICES, OptionalCollection, Producer, ProducerGap, elevated::{InputEvidence, VisitBinding}},
    },
    session::{AdmissionBoundary, hotkey::ControlOutcome, windows::{AwakeElapsed, ControlPrefix, InputFailure, Receipt, SuspendMarks}},
};

use super::{ProbeError, ReceiptPrefix, blind::Blinds, clock, clock_error, devices::Devices, gate::{Gate, Selection, is_key, record_loss}, hook_health::HookHealth, input_timing, resync, unavailable};
use super::super::{context::{ConfirmedText, Context, Visit}, elevated::InputCertificate, still_rows, stills};

/// A producer without progress for this long, suspend excluded, opens a
/// `producer_stall` blind interval (§6.2 row 11, G).
const STALL_BLIND: std::time::Duration = std::time::Duration::from_millis(250);
/// The REL-5 heartbeat interval: a stall this long is an input activity loss (L).
const STALL_LOSS: std::time::Duration = std::time::Duration::from_secs(5);
/// Resync not performed this long after its request opens `resync_unavailable`.
const RESYNC_BLIND_US: i64 = 250_000;
const SYNC_RETRY_US: i64 = 100_000;
/// Pointer rest after the last motion that triggers a pointer-rest still.
const POINTER_REST_US: i64 = 500_000;

/// The latest advance of one producer's progress, with the suspend sequence
/// read at or after it for suspend-excluded elapsed time.
#[derive(Clone, Copy)]
struct ProgressMark {
    qpc: QpcTick,
    time: SessionTimeUs,
    suspends: u64,
}

/// An open producer stall: the blind interval starts at the stalled
/// producers' last progress. It closes once each of them advances again.
struct Stall {
    start: SessionTimeUs,
    stalled: Vec<(Producer, QpcTick)>,
    escalated: bool,
}

pub(super) struct Receiver {
    matcher: InputMatcher,
    provisional: ProvisionalRequests,
    physical: PhysicalHeld,
    gate: Gate,
    sampler: MotionSampler,
    controls: ControlChords,
    hook_health: HookHealth,
    opened_at: Option<SessionTimeUs>,
    sync_after: Option<SessionTimeUs>,
    last_sync_attempt: Option<SessionTimeUs>,
    requires_opening_sync: bool,
    synchronized_qpc: Option<QpcTick>,
    sync_visit: Option<Visit>,
    overflow: bool,
    devices: BTreeMap<DeviceId, DeviceDescriptor>,
    device_inventory_truncated: bool,
    source: Option<VisitBinding>,
    source_certificate: Option<InputCertificate>,
    device_ids: Devices,
    past_hook_loss: (bool, bool),
    blinds: Blinds,
    /// The suspend sequence the pump read before this poll.
    suspends: u64,
    opening_mark: Option<ProgressMark>,
    marks: [Option<ProgressMark>; Producer::ALL.len()],
    stall: Option<Stall>,
    /// Input arrives while the receipt foreground is not ordinary-accessible.
    foreground_blind: bool,
    /// The records of each producer's pending foreground-inaccessible matcher
    /// gap, split by privacy class for its loss evidence.
    foreground_losses: [ProducerLoss; Producer::ALL.len()],
    optional_lost: Vec<OptionalCollection>,
    /// The latest standard-helper environment record seen while inactive. The
    /// helper checks its registrations before each one (E4).
    helper_checked: QpcTick,
}

impl Receiver {
    pub fn new(
        hotkey_keys: [u16; 4], endpoints: BTreeMap<MonitorId, stills::Requests>, rows: still_rows::Rows,
        injector_hints: Vec<ControllerHint>,
    ) -> Self {
        Self {
            matcher: InputMatcher::new(), provisional: ProvisionalRequests { endpoints, rows: Some(rows), ..ProvisionalRequests::default() }, overflow: false,
            physical: PhysicalHeld::default(), gate: Gate::new(injector_hints), sampler: MotionSampler::default(),
            controls: ControlChords::new(hotkey_keys),
            devices: BTreeMap::new(), device_inventory_truncated: false,
            hook_health: HookHealth::default(), opened_at: None, sync_after: None, last_sync_attempt: None,
            requires_opening_sync: true, synchronized_qpc: None, sync_visit: None,
            source: None, source_certificate: None, device_ids: Devices::default(), past_hook_loss: (false, false),
            blinds: Blinds::default(), suspends: 0, opening_mark: None, marks: [None; Producer::ALL.len()], stall: None,
            foreground_blind: false, foreground_losses: [ProducerLoss::default(); Producer::ALL.len()],
            optional_lost: Vec::new(), helper_checked: QpcTick(0),
        }
    }

    /// The suspend sequence read before the helper records of this poll.
    pub fn note_suspends(&mut self, sequence: u64) {
        self.suspends = sequence;
    }

    /// Producer liveness at `receipt.qpc`, sleep excluded (§6.2 row 11): a 250 ms
    /// stall opens a `producer_stall` blind interval at the last progress (§6.3);
    /// the producer of a 5 s stall is returned once for the activity loss, whose
    /// first missing receipt is [`Self::earliest_missing`]. An unknown suspend
    /// interval is not evaluated.
    pub fn check_stall(&mut self, receipt: Receipt, power: SuspendMarks) -> Result<Option<Producer>, ProbeError> {
        let now = receipt.clock.time(receipt.qpc).map_err(clock_error)?;
        // Liveness is measured from this (re)opening of ordinary input, which
        // can follow its admission boundary after an elevated visit.
        let opening = *self.opening_mark.get_or_insert(ProgressMark {
            qpc: receipt.qpc.max(receipt.boundary.qpc), time: now, suspends: power.sequence,
        });
        let mut stalled = Vec::new();
        let mut lost = None;
        for producer in Producer::ALL {
            let mark = self.marks[producer.index()].filter(|mark| mark.qpc >= opening.qpc).unwrap_or(opening);
            let AwakeElapsed::Known(awake) = power.awake_elapsed(mark.qpc, receipt.qpc, mark.suspends, receipt.clock.qpc_frequency)
            else { continue };
            if awake >= STALL_LOSS && lost.is_none() {
                lost = Some(producer);
            }
            if awake >= STALL_BLIND {
                stalled.push((producer, mark));
            }
        }
        if self.stall.is_none() && let Some(start) = stalled.iter().map(|(_, mark)| mark.time).min() {
            for (producer, mark) in &stalled {
                self.hook_health.begin_stall(*producer, mark.time);
            }
            self.begin_stall(start, now, stalled.iter().map(|(producer, mark)| (*producer, mark.qpc)).collect());
        }
        if let Some(stall) = &self.stall {
            // The open blind interval covers the stalled producers' receipts.
            for (producer, _) in &stall.stalled {
                self.matcher.progress(*producer, now);
            }
        }
        let Some(producer) = lost else { return Ok(None) };
        let Some(stall) = self.stall.as_mut().filter(|stall| !stall.escalated) else { return Ok(None) };
        stall.escalated = true;
        // The activity loss reports this interval through its own blind row.
        self.blinds.discard(InputBlindReason::ProducerStall);
        Ok(Some(producer))
    }

    fn begin_stall(&mut self, start: SessionTimeUs, now: SessionTimeUs, stalled: Vec<(Producer, QpcTick)>) {
        let producers: Vec<_> = stalled.iter().map(|(producer, _)| *producer).collect();
        self.blinds.open_for(InputBlindReason::ProducerStall, start, &producers);
        self.gate.note_health(HealthKind::InputProducerStall,
            Failure::new(FailureComponent::Receiver, "Input producer progress", None), now, start);
        // Records before the last progress are decided; later admitted ones
        // already held are counted. The held boundary and resync bracket the
        // interval; other producers' records keep being decided, with the
        // stalled producers covered as at expiry (§6.3).
        self.interrupt_inner(start, HeldBoundary::InputGap, None, false);
        self.request_sync(start);
        self.stall = Some(Stall { start, stalled, escalated: false });
    }

    /// Every stalled producer advanced past its stalled progress: the blind
    /// interval ends at this receipt and a gap and resync are required.
    fn end_stall(&mut self, end: SessionTimeUs) {
        if self.stall.as_ref().is_some_and(|stall| stall.escalated) { return; }
        self.stall = None;
        self.blinds.close(InputBlindReason::ProducerStall, end);
        self.matcher.invalidate_through(end);
        self.request_sync(end);
    }

    /// The earliest receipt an input activity loss can affect: the oldest
    /// producer progress or an open stall interval, never after `at`.
    pub fn earliest_missing(&self, at: SessionTimeUs) -> SessionTimeUs {
        [Some(at), self.matcher.certified_before(), self.stall.as_ref().map(|stall| stall.start)]
            .into_iter().flatten().min().unwrap_or(at)
    }

    /// The first missed receipt of a suspected hook loss.
    pub fn hook_lost_at(&self, producer: Producer) -> Option<SessionTimeUs> {
        self.hook_health.lost_at(producer)
    }

    /// The input health row of an input activity loss whose condition is an
    /// input health condition (INP-1, INP-12, §6.2 row 11).
    pub fn note_failure_health(&mut self, failure: &InputFailure) {
        let kind = match failure.reason {
            InputBlindReason::RegistrationLost => HealthKind::InputRegistrationLost,
            InputBlindReason::HookLost => HealthKind::InputHookLost,
            InputBlindReason::ProducerStall => HealthKind::InputProducerStall,
            _ => return,
        };
        self.gate.note_health(kind, failure.failure, failure.at, failure.first_missing);
    }

    /// Close every open blind interval at a closing boundary.
    pub fn close_blinds(&mut self, at: SessionTimeUs) {
        self.blinds.close_all(at);
    }

    /// Open a blind interval found by the pump, such as an uncertified tail.
    pub fn open_blind(&mut self, reason: InputBlindReason, start: SessionTimeUs) {
        self.blinds.open(reason, start);
    }

    pub fn optional_unavailable(&self) -> Vec<OptionalCollection> {
        self.optional_lost.clone()
    }

    fn note_optional_lost(&mut self, collection: OptionalCollection, at: Option<SessionTimeUs>) {
        if self.optional_lost.contains(&collection) { return; }
        self.optional_lost.push(collection);
        if let Some(at) = at {
            self.gate.note_health(HealthKind::OptionalCollectionUnavailable,
                Failure::new(FailureComponent::InputHelper, "Optional Raw Input registration", None), at, at);
        }
    }

    pub fn adopt_elevated_certificate(
        &mut self, certificate: InputCertificate, receipt: Receipt, context: &mut Context<'_>,
    ) -> bool {
        if self.source != Some(certificate.visit) || certificate.visit.epoch != receipt.boundary.epoch {
            return false;
        }
        if self.elevated_hook_loss().is_some_and(|before| certificate.before > before) {
            return false;
        }
        self.source_certificate = Some(certificate);
        context.certify_elevated_input(certificate);
        true
    }

    pub fn accept_elevated(
        &mut self, record: InputEvidence, receipt: Receipt, closing: Option<AdmissionBoundary>,
        context: &mut Context<'_>,
    ) -> Result<(), ProbeError> {
        let Some(certificate) = self.source_certificate else { return Ok(()); };
        if self.source != Some(certificate.visit) { return Ok(()); }
        let record = match record {
            InputEvidence::Input(value) if value.receipt.qpc < certificate.before => HelperToCore::Input(value),
            InputEvidence::DeviceArrived { receipt, descriptor } if receipt.qpc < certificate.before =>
                HelperToCore::DeviceArrived { receipt, descriptor },
            InputEvidence::DeviceRemoved { receipt, id } if receipt.qpc < certificate.before =>
                HelperToCore::DeviceRemoved { receipt, id },
            InputEvidence::Environment(state) if state.observed_qpc < certificate.before =>
                HelperToCore::Environment(state),
            InputEvidence::Gap(gap) if gap.last_qpc < certificate.before => HelperToCore::Gap(gap),
            InputEvidence::OptionalUnavailable { collection, observed_qpc } if observed_qpc < certificate.before =>
                HelperToCore::OptionalUnavailable { collection, observed_qpc },
            _ => return Ok(()),
        };
        self.accept(record, receipt, closing, context)
    }

    pub fn finish_elevated_delivery(&mut self, receipt: Receipt) -> Result<(), ProbeError> {
        let Some(certificate) = self.source_certificate else { return Ok(()); };
        for progress in certificate.producers {
            // The matcher may consume only the part of each original
            // producer bound materialized inside this certified delivery.
            let eligible = progress.before_qpc.min(certificate.before).min(certificate.context_before);
            let before = receipt.clock.time(eligible).map_err(clock_error)?;
            self.matcher.progress(progress.producer, before);
        }
        Ok(())
    }

    pub fn accept(
        &mut self, record: HelperToCore, receipt: Receipt, closing: Option<AdmissionBoundary>,
        context: &mut Context<'_>,
    ) -> Result<(), ProbeError> {
        let elevated = self.source.is_some();
        let input_receipt = match &record {
            HelperToCore::Input(packet) => Some((packet.sample.producer(), is_key(&packet.sample),
                packet.receipt.qpc, packet.receipt.message_tick_ms)),
            _ => None,
        };
        let record = self.device_ids.translate(elevated, record);
        if self.device_ids.incomplete(elevated) {
            self.device_inventory_truncated = true;
            self.gate.note_device_inventory_loss();
        }
        let Some(record) = record else {
            // Only an input record naming an absent device is discarded here.
            if let Some((producer, key, qpc, tick)) = input_receipt
                && closing.is_none_or(|closing| qpc < closing.qpc)
                && let Some(timing) = input_timing(receipt, qpc, tick)?
            {
                let at = timing.time.recv_t_us;
                self.gate.note_loss(record_loss(LossReason::UnknownDevice, producer, key, at, at));
            }
            return Ok(());
        };
        match record {
            HelperToCore::Input(packet) => {
                if closing.is_some_and(|closing| packet.receipt.qpc >= closing.qpc) {
                    return Ok(());
                }
                let inaccessible = if elevated { None } else {
                    packet.foreground_at_receipt.and_then(|proof| match context.foreground_access(proof.process) {
                        Some(screen_fix_elevated::ForegroundAccess::Ordinary) => None,
                        Some(screen_fix_elevated::ForegroundAccess::Elevated) => Some(InputBlindReason::ElevatedWindow),
                        None => Some(InputBlindReason::ForegroundInaccessible),
                    })
                };
                let Some(timing) = input_timing(receipt, packet.receipt.qpc, packet.receipt.message_tick_ms)? else {
                    if inaccessible.is_none() && packet.receipt.qpc < receipt.boundary.qpc && let Some(modifier) = startup_modifier(&packet) {
                        self.controls.note_startup_change(modifier);
                    }
                    return Ok(());
                };
                let producer = packet.sample.producer();
                let at = timing.time.recv_t_us;
                if let Some(reason) = inaccessible {
                    // Counted in the ordered input stream, where its gap also
                    // invalidates held state; the blind interval runs until
                    // return. A key keeps only its count (§6.1 rule 5).
                    self.blinds.open(reason, at);
                    self.foreground_blind = true;
                    let gap = InputGap::logical(producer, LossReason::ForegroundInaccessible, at);
                    self.foreground_losses[producer.index()].record(gap, is_key(&packet.sample));
                    self.matcher.gap(gap);
                    return Ok(());
                }
                if self.foreground_blind && packet.foreground_at_receipt.is_some() {
                    // Back on an accessible foreground: a resync is required first.
                    self.foreground_blind = false;
                    self.blinds.close(InputBlindReason::ElevatedWindow, at);
                    self.blinds.close(InputBlindReason::ForegroundInaccessible, at);
                    self.matcher.invalidate_through(at);
                    self.request_sync(at);
                }
                self.hook_health.note_callback(producer, at);
                if self.stall.as_ref().is_some_and(|stall| stall.stalled.iter().any(|(stalled, _)| *stalled == producer)) {
                    // Only the stalled producer's late records fall in its blind interval.
                    self.note_packet_lost(LossReason::ProducerStall, &packet.sample, at);
                    return Ok(());
                }
                if self.requires_opening_sync || self.synchronized_qpc.is_some_and(|after| packet.receipt.qpc <= after) {
                    // Counted with its receipt only; no payload is retained.
                    self.note_packet_lost(LossReason::ResyncPending, &packet.sample, at);
                    return Ok(());
                }
                if closing.is_none() && matches!(packet.sample,
                    crate::protocol::ipc::InputSample::RawPointer(_) | crate::protocol::ipc::InputSample::HookPointer(_))
                {
                    context.sample_cursor(true);
                }
                let layout = packet.foreground_at_receipt.and_then(|foreground| foreground.focus)
                    .map(|focus| focus.keyboard_layout);
                let qpc = packet.receipt.qpc;
                let window_geometry = packet.foreground_at_receipt.map_or(Evidence::Unavailable(UnavailableReason::NotObserved),
                    |proof| context.window_geometry_at(proof, qpc));
                let requests = &mut self.provisional;
                let source = self.source;
                self.matcher.push(packet, timing.epoch, timing.time, receipt.tick_anchor,
                    |vk| scan_from_layout(vk, layout),
                    |point| context.pointer_geometry_at(point, qpc),
                    window_geometry,
                    |change| requests.stage(change, source),
                ).map_err(|_| unavailable("The input request ID counter overflowed"))?;
            }
            HelperToCore::Progress(progress) => {
                let before = receipt.clock.time(progress.before_qpc).map_err(clock_error)?;
                self.matcher.progress(progress.producer, before);
                let mark = &mut self.marks[progress.producer.index()];
                if mark.is_none_or(|mark| progress.before_qpc > mark.qpc) {
                    *mark = Some(ProgressMark { qpc: progress.before_qpc, time: before, suspends: self.suspends });
                }
                if let Some(stall) = &mut self.stall {
                    let resumed = stall.stalled.iter().any(|(producer, stalled)| *producer == progress.producer && progress.before_qpc > *stalled);
                    if resumed {
                        stall.stalled.retain(|(producer, _)| *producer != progress.producer);
                        let end = receipt.clock.time(receipt.qpc).map_err(clock_error)?.max(before);
                        // This producer's own interval ends here; the shared
                        // blind interval ends when the last one resumes.
                        self.hook_health.end_stall(progress.producer, end);
                        if self.stall.as_ref().is_some_and(|stall| stall.stalled.is_empty()) {
                            self.end_stall(end);
                        }
                    }
                }
            }
            HelperToCore::OptionalUnavailable { collection, observed_qpc } => {
                let at = receipt.clock.time(observed_qpc.max(receipt.boundary.qpc)).map_err(clock_error)?;
                self.note_optional_lost(collection, Some(at));
            }
            HelperToCore::Gap(gap) => {
                self.inventory(HelperToCore::Gap(gap));
                if gap.last_qpc < receipt.boundary.qpc || closing.is_some_and(|end| gap.first_qpc >= end.qpc) {
                    return Ok(());
                }
                let first_qpc = gap.first_qpc.max(receipt.boundary.qpc);
                let last_qpc = closing.map_or(gap.last_qpc, |end| gap.last_qpc.min(end.qpc));
                let first = receipt.clock.time(first_qpc).map_err(clock_error)?;
                let last = receipt.clock.time(last_qpc).map_err(clock_error)?;
                let clipped = gap.first_qpc < receipt.boundary.qpc || closing.is_some_and(|end| gap.last_qpc > end.qpc);
                let count = native_count(gap, clipped);
                let reason = match gap.cause {
                    GapCause::Overflow => LossReason::NativeQueueOverflow,
                    GapCause::Malformed => LossReason::MalformedPacket,
                };
                self.matcher.gap(InputGap {
                    producer: gap.producer, reason, first, last, count,
                    source_seq: Some([gap.first_sequence.0, gap.last_sequence.0]), qpc: Some([first_qpc, last_qpc]),
                });
            }
            HelperToCore::DeviceRemoved { receipt: removal, id } => {
                if removal.qpc >= receipt.boundary.qpc && closing.is_none_or(|end| removal.qpc < end.qpc) {
                    let time = receipt.clock.time(removal.qpc).map_err(clock_error)?;
                    self.matcher.remove_device(id, removal, time);
                } else if removal.qpc < receipt.boundary.qpc {
                    self.gate.forget_input_device(id);
                }
            }
            record => self.inventory(record),
        }
        Ok(())
    }

    /// A standard-source record after the source-change latch (E1). An admitted
    /// input record never reaches the matcher: it is counted and covered by a
    /// blind interval until the transition boundary closes it.
    pub fn drain_after_latch(&mut self, record: HelperToCore, receipt: Receipt) -> Result<(), ProbeError> {
        if let HelperToCore::Input(packet) = &record {
            if let Some(timing) = input_timing(receipt, packet.receipt.qpc, packet.receipt.message_tick_ms)? {
                let at = timing.time.recv_t_us;
                self.blinds.open(InputBlindReason::ElevatedWindow, at);
                self.note_packet_lost(LossReason::ForegroundInaccessible, &packet.sample, at);
            }
            return Ok(());
        }
        self.collect_ordinary_inventory(record, true);
        Ok(())
    }

    /// An elevated record past the certified prefix of an interrupted visit (E1).
    pub fn note_uncertified(&mut self, record: InputEvidence, receipt: Receipt) -> Result<(), ProbeError> {
        if let InputEvidence::Input(packet) = record && packet.receipt.qpc >= receipt.boundary.qpc {
            let at = receipt.clock.time(packet.receipt.qpc).map_err(clock_error)?;
            self.note_packet_lost(LossReason::UncertifiedSource, &packet.sample, at);
        }
        Ok(())
    }

    pub fn countdown(&mut self, record: HelperToCore) {
        if let HelperToCore::Input(input) = &record && let Some(modifier) = startup_modifier(input) {
            self.controls.note_startup_change(modifier);
        }
        self.collect_ordinary_inventory(record, true);
    }

    /// When the inactive standard helper last attested its registrations.
    pub fn helper_checked(&self) -> QpcTick { self.helper_checked }

    pub fn collect_ordinary_inventory(&mut self, record: HelperToCore, selected: bool) {
        if let HelperToCore::Environment(state) = &record {
            self.helper_checked = self.helper_checked.max(state.observed_qpc);
        }
        if let HelperToCore::OptionalUnavailable { collection, .. } = record {
            // Outside admission the loss is kept for the manifest only.
            self.note_optional_lost(collection, None);
            return;
        }
        if !matches!(&record, HelperToCore::DeviceArrived { .. } | HelperToCore::DeviceRemoved { .. }
            | HelperToCore::Gap(crate::protocol::ipc::ProducerGap {
                producer: Producer::DeviceNotifications, ..
            }))
        {
            return;
        }
        if let Some(mapped) = self.device_ids.translate(false, record) && selected {
            self.inventory(mapped);
        }
    }

    pub fn select_source(&mut self, source: Option<VisitBinding>) {
        if self.source == source { return; }
        self.past_hook_loss = self.hook_loss();
        self.hook_health = HookHealth::default();
        if let Some(stall) = &self.stall {
            // An open stall still explains the missing hook partners.
            for (producer, _) in &stall.stalled {
                self.hook_health.begin_stall(*producer, stall.start);
            }
        }
        self.source = source;
        self.source_certificate = None;
        if source.is_some() {
            self.device_ids.new_elevated_owner();
        }
        self.gate.reset_device_inventory();
        if self.device_ids.incomplete(source.is_some()) {
            self.device_inventory_truncated = true;
            self.gate.note_device_inventory_loss();
        }
        let current: Vec<_> = self.device_ids.current(source.is_some()).cloned().collect();
        for descriptor in current {
            self.add_device(descriptor);
        }
    }

    pub fn accept_controls(&mut self, prefix: &ControlPrefix, receipt: Receipt) -> Result<(), ProbeError> {
        for notice in prefix.notices() {
            if matches!(notice.outcome, ControlOutcome::StartRequested)
                || (notice.outcome == ControlOutcome::ResumeRequested && notice.activation.at <= receipt.boundary.qpc)
            {
                self.controls.start_chord();
            } else {
                let at = receipt.clock.time(notice.activation.at).map_err(clock_error)?;
                self.controls.confirm(notice.activation, at);
            }
        }
        if let Some(loss) = prefix.loss {
            self.overflow = true;
            self.controls.note_loss(receipt.clock.time(loss.last).map_err(clock_error)?);
        }
        if let Some(before) = prefix.before {
            self.controls.progress(receipt.clock.time(before).map_err(clock_error)?);
        }
        if prefix.failed {
            let unavailable_at = receipt.clock.time(receipt.qpc).map_err(clock_error)?;
            self.controls.note_loss(unavailable_at);
            self.controls.progress(unavailable_at);
        }
        Ok(())
    }

    pub fn inventory(&mut self, record: HelperToCore) {
        match record {
            HelperToCore::DeviceArrived { descriptor, .. } => self.add_device(descriptor),
            HelperToCore::DeviceRemoved { id, .. } => self.gate.forget_input_device(id),
            HelperToCore::Gap(gap) if gap.producer == crate::protocol::ipc::Producer::DeviceNotifications => {
                self.device_inventory_truncated = true;
                self.gate.note_device_inventory_loss();
            }
            _ => {}
        }
    }

    /// An active device. The gate always learns it; only the manifest
    /// inventory is bounded, and a device beyond it truncates the inventory.
    fn add_device(&mut self, descriptor: DeviceDescriptor) {
        self.gate.note_input_device(&descriptor);
        if self.devices.len() < MAX_INPUT_DEVICES || self.devices.contains_key(&descriptor.id) {
            self.devices.insert(descriptor.id, descriptor);
        } else {
            self.device_inventory_truncated = true;
        }
    }

    pub fn finish_still_prefix(&mut self, through: SessionTimeUs) {
        self.gate.resolve_confirmed(&mut self.provisional);
        self.provisional.advance(SessionTimeUs(through.0.saturating_add(1)), false);
    }

    pub fn open_elevated_stills(&mut self, source: VisitBinding, before: SessionTimeUs) {
        self.provisional.open_deferred(source, before);
    }

    pub fn advance_stills(&self, through: SessionTimeUs, input_live: bool) {
        if !input_live {
            self.provisional.advance(through, false);
        } else if let Some(before) = self.unresolved_before() {
            self.provisional.advance(before.min(through), true);
        }
    }

    pub fn poll_pointer_rest(&mut self, context: &mut Context<'_>, receipt: Receipt, observed: QpcTick)
        -> Result<(), ProbeError>
    {
        self.provisional.poll_pointer_rest(context, receipt, observed, self.source)
    }

    pub fn pen_observation(&self) -> (bool, bool) {
        self.gate.pen_observation()
    }

    pub fn finish_inventory(&mut self) -> Result<(BoundedList<DeviceDescriptor, MAX_INPUT_DEVICES>, bool), ProbeError> {
        let inventory = BoundedList::new(std::mem::take(&mut self.devices).into_values().collect())
            .map_err(|_| unavailable("The device inventory exceeds the input limit"))?;
        Ok((inventory, self.device_inventory_truncated))
    }

    pub fn advance(&mut self, admitted_through: SessionTimeUs) {
        if self.opened_at.is_none() {
            self.opened_at = Some(admitted_through);
            self.sync_after = Some(admitted_through);
        }
        while self.elevated_hook_loss().is_none()
            && let Some(step) = self.matcher.ready(admitted_through)
        {
            self.consume(step);
        }
        if self.elevated_hook_loss().is_some() { return; }
        if let Some(bound) = self.matcher.unresolved_before() {
            self.process_samples(|sampler, emit| sampler.advance(bound.min(admitted_through), emit));
        }
        self.release_controls(self.sampled_before().map(|bound| bound.min(admitted_through)), admitted_through, false);
    }

    pub fn try_synchronize(&mut self, receipt: Receipt, now: SessionTimeUs, context: &Context<'_>) -> Result<(), ProbeError> {
        // A resync that cannot be performed promptly is a blind interval from its
        // request until it succeeds. Stall and foreground intervals already cover theirs.
        if self.requires_opening_sync && self.stall.is_none() && !self.foreground_blind
            && let Some(after) = self.sync_after.filter(|after| now.0.saturating_sub(after.0) >= RESYNC_BLIND_US)
        {
            self.blinds.open(InputBlindReason::ResyncUnavailable, after);
        }
        if self.sync_after.is_none_or(|after| now < after)
            || self.last_sync_attempt.is_some_and(|last| now.0.saturating_sub(last.0) < SYNC_RETRY_US)
        {
            return Ok(());
        }
        self.last_sync_attempt = Some(now);
        let visit = context.visit().filter(|&visit| context.confirm_focus(visit));
        if let Some(state) = resync::read() {
            let qpc = clock::qpc()?;
            let at = receipt.clock.time(qpc).map_err(clock_error)?;
            self.install_synchronization(qpc, at, state, visit.filter(|&visit| context.confirm_focus(visit)));
        }
        Ok(())
    }

    pub fn elevated_synchronization(
        &mut self, receipt: Receipt, completed: QpcTick, snapshot: Option<screen_fix_protocol::ipc::elevated::HeldSnapshot>,
        visit: Visit,
    ) -> Result<(), ProbeError> {
        let at = receipt.clock.time(completed).map_err(clock_error)?;
        if let Some(snapshot) = snapshot {
            self.install_synchronization(completed, at, resync::from_snapshot(snapshot), Some(visit));
        } else {
            self.requires_opening_sync = false;
            self.synchronized_qpc = Some(completed);
            self.controls.note_loss(at);
            self.blinds.close(InputBlindReason::ResyncUnavailable, at);
        }
        Ok(())
    }

    pub fn recovered_synchronization(
        &mut self, receipt: Receipt, completed: QpcTick, snapshot: screen_fix_protocol::ipc::elevated::HeldSnapshot,
        visit: Visit,
    ) -> Result<(), ProbeError> {
        let at = receipt.clock.time(completed).map_err(clock_error)?;
        self.sync_visit = Some(visit);
        self.matcher.stage_synchronization(completed, at, resync::from_snapshot(snapshot));
        Ok(())
    }

    pub fn note_native_gap(&mut self) { self.overflow = true; }

    /// The input-stream marker of an elevated input loss or staging overflow
    /// (E3), next to its context `input_source_loss` row. A native range keeps
    /// its producer, sequences and count; coverage without one has no count.
    pub fn note_elevated_loss(
        &mut self, reason: LossReason, gap: Option<ProducerGap>, before: QpcTick, receipt: Receipt,
    ) -> Result<(), ProbeError> {
        let (first_qpc, last_qpc) = gap.map_or((before, before), |gap| (gap.first_qpc, gap.last_qpc));
        if last_qpc < receipt.boundary.qpc { return Ok(()); }
        let clipped = first_qpc < receipt.boundary.qpc;
        let first_qpc = first_qpc.max(receipt.boundary.qpc);
        let count = gap.map_or(LossCount::Unknown, |gap| native_count(gap, clipped));
        self.gate.note_loss(LossEvidence {
            reason, producer: gap.map(|gap| LossProducer::from(gap.producer)), count,
            scope: LossScope::Ordinary(LossRange {
                first_t_us: receipt.clock.time(first_qpc).map_err(clock_error)?,
                last_t_us: receipt.clock.time(last_qpc).map_err(clock_error)?,
                source_seq: gap.map(|gap| [gap.first_sequence.0, gap.last_sequence.0]),
                qpc: Some([first_qpc, last_qpc]),
            }),
        });
        Ok(())
    }

    /// The helper drained and exited normally: every producer delivered its
    /// final records, so closing decisions are exact (§6.3).
    pub fn close_producers(&mut self) {
        self.matcher.close_producers();
    }

    /// One admitted packet discarded before matching. It keeps only its
    /// receipt, and a key only its count (§6.1 rule 5).
    fn note_packet_lost(&mut self, reason: LossReason, sample: &InputSample, at: SessionTimeUs) {
        self.gate.note_loss(record_loss(reason, sample.producer(), is_key(sample), at, at));
    }

    /// The loss evidence of a matcher gap. A foreground-inaccessible gap takes
    /// its evidence from the per-producer split, so its keys stay count-only.
    fn note_gap_loss(&mut self, gap: InputGap) {
        if gap.reason == LossReason::ForegroundInaccessible {
            std::mem::take(&mut self.foreground_losses[gap.producer.index()])
                .note(&mut self.gate, gap.reason, gap.producer);
        } else {
            self.gate.note_loss(gap.evidence());
        }
    }

    fn install_synchronization(
        &mut self, qpc: QpcTick, at: SessionTimeUs, state: crate::capture::input::HeldSynchronization, visit: Option<Visit>,
    ) {
        self.sync_visit = visit;
        self.matcher.stage_synchronization(qpc, at, state);
        self.requires_opening_sync = false;
        self.synchronized_qpc = Some(qpc);
        self.sync_after = None;
        self.blinds.close(InputBlindReason::ResyncUnavailable, at);
    }

    fn sampled_before(&self) -> Option<SessionTimeUs> {
        let matched = self.matcher.unresolved_before()?;
        Some(self.sampler.unresolved_before().map_or(matched, |sample| sample.min(matched)))
    }

    fn matched_and_sampled_before(&self) -> Option<SessionTimeUs> {
        Some(self.sampled_before()?.min(self.controls.local_before()?))
    }

    pub fn unresolved_before(&self) -> Option<SessionTimeUs> {
        let bound = self.matched_and_sampled_before()?;
        Some(self.gate.unresolved_before().map_or(bound, |pending| bound.min(pending)))
    }

    pub fn stage_confirmed(&mut self, text: ConfirmedText, receipt: Receipt) {
        self.gate.stage_confirmed(text, receipt);
    }

    /// Loss evidence found outside the receiver, such as a text commit lost
    /// before confirmation (I7) or undecodable elevated records (E3).
    pub fn note_loss(&mut self, evidence: LossEvidence) {
        self.gate.note_loss(evidence);
    }

    pub fn publish(
        &mut self, context: &mut Context<'_>, output: &mut JournalIngress, prefix: &mut ReceiptPrefix,
        receipt: Receipt, through: SessionTimeUs, final_boundary: bool,
    ) -> Result<(), ProbeError> {
        let certified = self.matched_and_sampled_before();
        let certified = if let Some(before) = self.elevated_hook_loss() {
            let at = receipt.clock.time(before).map_err(clock_error)?;
            certified.map(|bound| bound.min(at))
        } else { certified };
        self.gate.drain(context, output, prefix, &mut self.provisional, &mut self.blinds, receipt, through, certified, final_boundary)
    }

    pub fn synchronization_qpc(&self) -> Option<QpcTick> {
        self.synchronized_qpc
    }

    pub fn uncertified_tail(&self, opened: SessionTimeUs, cutoff: SessionTimeUs, source_before: Option<SessionTimeUs>) -> Option<UncertifiedInputTail> {
        let first_t_us = self.matcher.certified_before().unwrap_or(opened);
        let first_t_us = source_before.map_or(first_t_us, |before| before.min(first_t_us)).max(opened);
        // Ordinary closing admits the cutoff itself. Elevated lane coverage
        // closes strictly before it, so equality leaves no elevated tail.
        (first_t_us < cutoff || (source_before.is_none() && first_t_us == cutoff))
            .then_some(UncertifiedInputTail { first_t_us, cutoff_t_us: cutoff, reason: None })
    }

    pub fn certified_through(&self, cutoff: SessionTimeUs) -> bool {
        self.matcher.certified_before().is_some_and(|bound| bound > cutoff)
    }

    pub fn reopen(&mut self, at: SessionTimeUs, hotkey: bool) {
        self.matcher.reopen();
        self.opened_at = Some(at);
        self.sync_after = Some(at);
        self.last_sync_attempt = None;
        self.requires_opening_sync = true;
        self.synchronized_qpc = None;
        self.sync_visit = None;
        self.source_certificate = None;
        self.hook_health.reopen();
        self.provisional.motion = None;
        self.controls.reopen(hotkey);
        self.opening_mark = None;
        self.marks = [None; Producer::ALL.len()];
        self.stall = None;
        self.foreground_blind = false;
        self.foreground_losses = Default::default();
    }

    pub fn interrupt(&mut self, cutoff: SessionTimeUs, boundary: HeldBoundary) {
        self.interrupt_inner(cutoff, boundary, matches!(boundary, HeldBoundary::Pause | HeldBoundary::Stop).then_some(cutoff), true);
    }

    pub fn interrupt_certified(&mut self, cutoff: SessionTimeUs, close_at: SessionTimeUs, boundary: HeldBoundary) {
        self.interrupt_inner(close_at, boundary, Some(cutoff), true);
    }

    /// End the certified prefix, the records strictly before `cutoff`, at
    /// `close_at`; each is decided with the candidates present (§6.3). Admitted
    /// records outside it are lost over a range from the cutoff. When `closing`,
    /// nothing at or after `close_at` was admitted; otherwise a blind interval
    /// continues and every later record is admitted.
    fn interrupt_inner(&mut self, close_at: SessionTimeUs, boundary: HeldBoundary, certificate: Option<SessionTimeUs>, closing: bool) {
        let cutoff = certificate.unwrap_or(close_at);
        if self.matcher.certified_before().is_none_or(|bound| bound <= cutoff) {
            self.controls.note_loss(cutoff);
        }
        let limit = SessionTimeUs(cutoff.0.saturating_sub(1));
        while self.elevated_hook_loss().is_none()
            && let Some(step) = self.matcher.finish(limit)
        { self.consume(step); }
        self.process_samples(|sampler, emit| sampler.flush(emit));
        self.release_controls(Some(cutoff), cutoff, true);
        self.controls.close_interval(cutoff);
        self.physical.invalidate();
        let hook_before = self.elevated_hook_loss();
        if let Some(before) = certificate {
            self.gate.retain_prefix(before, close_at, hook_before, &mut self.provisional);
        }
        if hook_before.is_some() {
            // The elevated hook loss is a blind interval from the first missed receipt.
            let lost: Vec<_> = [(self.hook_health.keyboard_lost(), Producer::KeyboardHook), (self.hook_health.pointer_lost(), Producer::PointerHook)]
                .into_iter().filter_map(|(lost, producer)| lost.then_some(producer)).collect();
            self.blinds.open_for(InputBlindReason::HookLost, self.hook_health.first_lost_at().unwrap_or(cutoff).min(close_at), &lost);
            self.blinds.close(InputBlindReason::HookLost, close_at);
            self.gate.boundary(close_at, HeldBoundary::HookUnavailable, Selection::All);
        }
        self.gate.boundary(close_at, boundary, Selection::All);
        let reason = |input: &PendingInput| if hook_before.is_some_and(|before| input.packet.receipt.qpc >= before) {
            LossReason::HookLost
        } else {
            LossReason::ProducerStall
        };
        let admitted = |at: SessionTimeUs| !closing || at < close_at;
        let Discarded { pending, decided, gaps } = self.matcher.discard();
        // A decided row is one item, accounted through its first record.
        for input in decided.iter().map(|row| &row.first).chain(&pending).filter(|input| admitted(input.time.recv_t_us)) {
            self.gate.note_records_lost(reason(input), [input], Some(cutoff));
        }
        for gap in gaps.into_iter().filter(|gap| admitted(gap.first)) {
            self.note_gap_loss(gap);
        }
        // The matcher holds no gap now; the splits of unadmitted ones go with it.
        self.foreground_losses = Default::default();
        self.provisional.resolve_records(pending.iter().chain(decided.iter().flat_map(|row| row.records())));
    }

    pub fn overflowed(&self) -> bool {
        self.overflow || self.gate.overflowed()
    }

    pub fn elevated_hook_cutoff(&self) -> Option<(VisitBinding, QpcTick)> {
        self.source.zip(self.hook_health.lost_before())
    }

    pub fn elevated_hook_loss(&self) -> Option<QpcTick> {
        self.source.and_then(|_| self.hook_health.lost_before())
    }

    pub fn frame_before(&self) -> Option<SessionTimeUs> {
        self.matcher.unresolved_before()
    }

    pub fn take_hook_reports(&mut self) -> [Option<Producer>; 2] {
        self.hook_health.take_reports()
    }

    pub fn hook_loss(&self) -> (bool, bool) {
        (self.past_hook_loss.0 || self.hook_health.keyboard_lost(),
            self.past_hook_loss.1 || self.hook_health.pointer_lost())
    }

    fn consume(&mut self, step: InputStep) {
        match step {
            InputStep::Record(input) => {
                // Absorbed records are represented by their row (raw motion stays
                // for its evidence); cluster excess hooks count once, via the first row.
                self.provisional.resolve_records(&input.absorbed);
                if let Some(cluster) = input.cluster {
                    self.gate.note_suppressed(SuppressionClass::ClusterExcessHook, u64::from(cluster.excess_hooks));
                }
                // Ordinary handling keeps its original HID and hook boundary order.
                let elevated = self.source.is_some();
                if elevated {
                    self.hook_health.note_correlation(&input);
                    if self.elevated_hook_loss().is_some() {
                        self.gate.note_records_lost(LossReason::HookLost, [&input.first], None);
                        self.provisional.resolve_records(input.records());
                        return;
                    }
                }
                if let InputSample::Hid(report) = &input.first.packet.sample && report.pen_continuity_uncertain() {
                    self.flush_motion();
                    let at = input.first.time.recv_t_us;
                    self.release_controls(Some(at), at, true);
                    self.physical.invalidate_pen(report.device);
                    self.gate.boundary(input.first.time.recv_t_us, HeldBoundary::HidReportUnavailable,
                        Selection::PenDevice(report.device));
                }
                if !elevated {
                    let prior_loss = (self.hook_health.keyboard_lost(), self.hook_health.pointer_lost());
                    self.hook_health.note_correlation(&input);
                    if (self.hook_health.keyboard_lost(), self.hook_health.pointer_lost()) != prior_loss {
                        self.invalidate(input.first.time.recv_t_us, input.first.time.recv_t_us, HeldBoundary::HookUnavailable);
                    }
                }
                let pointer_lost = self.hook_health.pointer_lost();
                let normalized = NormalizedInput::new(input, pointer_lost, key_name);
                let held = self.physical.observe(normalized);
                self.process_samples(|sampler, emit| sampler.push(held, emit));
            }
            InputStep::Gap(gap) if gap.producer == Producer::OptionalHid => {
                // Optional HID loss keeps keyboard and pointer state.
                self.overflow = true;
                self.provisional.note_gap(gap);
                self.note_gap_loss(gap);
                self.gate.reset_optional_hid();
            }
            InputStep::Gap(gap) => {
                self.overflow = true;
                self.hook_health.reset_streaks();
                self.flush_motion();
                self.apply_gap(gap.first, gap.last, HeldBoundary::InputGap, [Some(gap)]);
                self.note_gap_loss(gap);
            }
            InputStep::Undecidable { records, since } => {
                // Keys of an unclassified visit are counted without times.
                self.gate.note_records_lost(LossReason::ProducerStall, &records, Some(since));
                self.provisional.resolve_records(&records);
            }
            InputStep::DeviceRemoved { id, at } => {
                self.flush_motion();
                self.controls.note_loss(at);
                self.release_controls(Some(at), at, true);
                self.controls.close_interval(at);
                self.physical.remove(id);
                self.gate.boundary(at, HeldBoundary::DeviceRemoved, Selection::Removed(id));
                // Held state is unknown from the removal until the next resync.
                self.blinds.open(InputBlindReason::DeviceRemoved, at);
                self.request_sync(at);
            }
            InputStep::Synchronize { state, at } => {
                if self.flush_motion() {
                    self.request_sync(at);
                    return;
                }
                self.blinds.close(InputBlindReason::DeviceRemoved, at);
                self.release_controls(Some(at), at, true);
                let omitted = self.controls.omitted_from_sync(state);
                let mut edges = self.physical.synchronize(state, at);
                edges.retain(|edge| modifier_of(edge.edge.identity).is_none_or(|modifier| omitted & modifier_bit(modifier) == 0));
                if let Some(visit) = self.sync_visit.take() {
                    let eligible_modifiers = !omitted;
                    self.gate.synchronize(at, visit,
                        state.modifiers & eligible_modifiers, !state.modifiers & eligible_modifiers, edges);
                } else {
                    self.gate.note_suppressed(SuppressionClass::UnconfirmedSynchronization, edges.len() as u64);
                }
            }
        }
    }

    fn request_sync(&mut self, through: SessionTimeUs) {
        self.matcher.discard_stale_synchronization(through);
        self.requires_opening_sync = true;
        self.sync_visit = None;
        self.sync_after = Some(self.sync_after.map_or(through, |previous| previous.max(through)));
    }

    fn invalidate(&mut self, at: SessionTimeUs, through: SessionTimeUs, reason: HeldBoundary) {
        self.flush_motion();
        self.apply_gap(at, through, reason, []);
    }

    /// Invalidate held state over a loss from `at` through `through`. The
    /// caller records the loss evidence of `gaps`.
    fn apply_gap(
        &mut self, at: SessionTimeUs, through: SessionTimeUs, reason: HeldBoundary,
        gaps: impl IntoIterator<Item = Option<InputGap>>,
    ) {
        self.controls.note_loss(through);
        self.release_controls(Some(at), at, true);
        for gap in gaps.into_iter().flatten() {
            self.provisional.note_gap(gap);
        }
        self.controls.close_interval(through);
        self.physical.invalidate();
        self.gate.boundary(at, reason, Selection::All);
        self.request_sync(through);
    }

    fn flush_motion(&mut self) -> bool {
        self.process_samples(|sampler, emit| sampler.flush(emit))
    }

    /// A failed batch cannot continue staging records with pre-gap snapshots.
    /// Take unresolved sampler output once, preserving the earlier prefix,
    /// then apply a single boundary
    /// without re-entering either bounded queue.
    fn process_samples(
        &mut self, run: impl FnOnce(&mut MotionSampler, &mut dyn FnMut(Sampled)),
    ) -> bool {
        let mut losses = SampleLoss::default();
        let requests = &mut self.provisional;
        let controls = &mut self.controls;
        run(&mut self.sampler, &mut |sample| {
            match sample {
                Sampled::Retained(input) if losses.interval.is_none() => {
                    if let Err(input) = controls.stage(input) {
                        losses.record(input, requests);
                    }
                }
                Sampled::Thinned(mut input) if losses.interval.is_none() => {
                    input.motion_thinned = true;
                    if let Err(input) = controls.stage(input) {
                        losses.record(input, requests);
                    }
                }
                Sampled::Retained(input) | Sampled::Dropped(input) | Sampled::Thinned(input) => {
                    losses.record(input, requests);
                }

            }
        });
        if losses.interval.is_none() {
            return false;
        }
        for input in self.sampler.discard_pending() {
            let at = input.normalized.correlated.first.time.recv_t_us;
            if losses.interval.is_some_and(|(first, _)| at < first) {
                if let Err(input) = self.controls.stage(input) {
                    losses.record(input, &mut self.provisional);
                }
            } else {
                losses.record(input, &mut self.provisional);
            }
        }
        let Some((first, _)) = losses.interval else { return false };
        for input in self.controls.discard_from(first) {
            losses.record(input, &mut self.provisional);
        }
        let Some((first, last)) = losses.interval else { return false };
        self.overflow = true;
        self.hook_health.reset_streaks();
        self.matcher.invalidate_through(last);
        self.apply_gap(first, last, HeldBoundary::InputGap, losses.by_producer);
        losses.note(&mut self.gate);
        true
    }

    fn release_controls(&mut self, before: Option<SessionTimeUs>, at: SessionTimeUs, closing: bool) {
        for classified in self.controls.release(before, at, closing) {
            if let Some(boundary) = classified.boundary && (!closing || boundary.at <= at) {
                self.gate.boundary(classified.input.normalized.correlated.first.time.recv_t_us,
                    HeldBoundary::RecorderControl, Selection::Modifiers(boundary.modifiers));
            }
            self.gate.push(classified, &mut self.provisional);
        }
    }
}

/// The record count of a native sequence range. A clipped range also covers
/// records outside this admission, so its count is unknown.
fn native_count(gap: ProducerGap, clipped: bool) -> LossCount {
    std::num::NonZeroU64::new(gap.last_sequence.0.saturating_sub(gap.first_sequence.0).saturating_add(1))
        .filter(|_| !clipped).map_or(LossCount::Unknown, LossCount::Known)
}

/// The lost records of one producer split by privacy class (§6.1 rule 5):
/// key records keep only their count, since their visit may be sensitive;
/// the others keep their merged receipt range.
#[derive(Clone, Copy, Default)]
struct ProducerLoss {
    keys: u64,
    others: Option<InputGap>,
}

impl ProducerLoss {
    /// One lost record, as its logical gap.
    fn record(&mut self, gap: InputGap, key: bool) {
        if key {
            self.keys = self.keys.saturating_add(1);
        } else {
            self.others = Some(self.others.map_or(gap, |old| old.merge(gap)));
        }
    }

    fn note(self, gate: &mut Gate, reason: LossReason, producer: Producer) {
        if let Some(count) = std::num::NonZeroU64::new(self.keys) {
            gate.note_loss(LossEvidence::sensitive(reason, Some(LossProducer::from(producer)), count));
        }
        if let Some(gap) = self.others {
            gate.note_loss(gap.evidence());
        }
    }
}

#[derive(Default)]
struct SampleLoss {
    interval: Option<(SessionTimeUs, SessionTimeUs)>,
    /// Every lost record of each producer, for the held invalidation.
    by_producer: [Option<InputGap>; Producer::ALL.len()],
    /// The same records split by privacy class, for their loss evidence.
    evidence: [ProducerLoss; Producer::ALL.len()],
}

impl SampleLoss {
    fn record(&mut self, input: HeldInput, requests: &mut ProvisionalRequests) {
        let slot = &input.normalized.correlated.first;
        let at = slot.time.recv_t_us;
        let producer = slot.packet.sample.producer();
        let gap = InputGap::logical(producer, LossReason::StagingPressure, at);
        let pending = &mut self.by_producer[producer.index()];
        *pending = Some(pending.map_or(gap, |old| old.merge(gap)));
        self.evidence[producer.index()].record(gap, is_key(&slot.packet.sample));
        self.interval = Some(self.interval.map_or((at, at), |(first, last)| (first.min(at), last.max(at))));
        requests.retire_input(input);
    }

    fn note(&self, gate: &mut Gate) {
        for producer in Producer::ALL {
            self.evidence[producer.index()].note(gate, LossReason::StagingPressure, producer);
        }
    }
}

fn scan_from_layout(vk: u16, layout: Option<u64>) -> Option<ScanCode> {
    let layout = usize::try_from(layout?).ok()? as HKL;
    let scan = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC_EX, layout) };
    let code = (scan & 0xff) as u8;
    if code == 0 {
        return None;
    }
    let prefix = match scan >> 8 {
        0 => ScanPrefix::None,
        0xe0 => ScanPrefix::E0,
        0xe1 => ScanPrefix::E1,
        _ => return None,
    };
    Some(ScanCode { prefix, code })
}

fn key_name(key: Key, layout: Option<u64>) -> Option<String> {
    let fixed = match key.virtual_key {
        0x08 => "backspace",
        0x09 => "tab",
        0x0d => "enter",
        0x13 => "pause",
        0x14 => "caps_lock",
        0x1b => "escape",
        0x20 => "space",
        0x21 => "page_up",
        0x22 => "page_down",
        0x23 => "end",
        0x24 => "home",
        0x25 => "left",
        0x26 => "up",
        0x27 => "right",
        0x28 => "down",
        0x2c => "print_screen",
        0x2d => "insert",
        0x2e => "delete",
        0x5b => "win_l",
        0x5c => "win_r",
        0x5d => "menu",
        0x70..=0x87 => return Some(format!("f{}", key.virtual_key - 0x6f)),
        0x90 => "num_lock",
        0x91 => "scroll_lock",
        0xa0 => "shift_l",
        0xa1 => "shift_r",
        0xa2 => "ctrl_l",
        0xa3 => "ctrl_r",
        0xa4 => "alt_l",
        0xa5 => "alt_r",
        0xe7 => "packet",
        vk => {
            let layout = usize::try_from(layout?).ok()? as HKL;
            let mapped = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_CHAR, layout) } & 0x7fff_ffff;
            return char::from_u32(mapped).filter(|ch| !ch.is_control())
                .map(|ch| ch.to_lowercase().collect());
        }
    };
    Some(fixed.to_owned())
}

/// This map owns the private envelopes for the bounded input prefix. P10
/// can consume the same opening/resolution edges to drive actual retention.
#[derive(Default)]
pub(super) struct ProvisionalRequests {
    pending: BTreeMap<ObservationId, PrivateRequest>,
    endpoints: BTreeMap<MonitorId, stills::Requests>,
    rows: Option<still_rows::Rows>,
    motion: Option<MotionRest>,
    /// The latest motion receipt, for the IMG-2 pre-motion idle condition.
    last_motion: Option<SessionTimeUs>,
}

/// Motion idle time before a motion opens a pre-motion observation (IMG-2).
const MOTION_IDLE_US: i64 = 300_000;

struct MotionRest {
    point: PhysicalPoint,
    geometry: GeometryId,
    proof: ForegroundAtReceipt,
    focus: FocusGeneration,
    admission: AdmissionEpoch,
    last: SessionTimeUs,
}

struct PrivateRequest {
    envelope: ProvisionalInputObservation,
    decision: Option<SyncSender<stills::Permission>>,
    deferred_source: Option<VisitBinding>,
    rows: Option<stills::EarlyRows>,
}

impl ProvisionalRequests {
    fn advance(&self, before: SessionTimeUs, input_live: bool) {
        let local = self.pending.values().map(|entry| entry.envelope.time.recv_t_us).min();
        let certified = local.map_or(before, |local| local.min(before));
        if let Some(rows) = &self.rows {
            rows.advance(certified);
        }
        if input_live {
            for endpoint in self.endpoints.values() {
                endpoint.input_progress(certified);
            }
        }
    }

    pub fn approved_observation_gap(&self, at: SessionTimeUs, geometry: Evidence<GeometryId>, kind: Option<ProvisionalInputKind>,
        observation: Option<ObservationId>,
    ) {
        if let Some(kind) = kind {
            self.mark_action(at, geometry, kind);
        }
        // A motion that opened no pre-motion envelope reserved no still rows.
        let reserved = kind != Some(ProvisionalInputKind::PreMotion)
            || observation.and_then(|id| self.pending.get(&id)).is_some_and(|entry| entry.rows.is_some());
        if reserved && let Some(rows) = &self.rows && let Some(kind) = kind {
            // The gate had no confirmation capacity for the approved action.
            for _ in 0..stills::trigger_rows(kind) {
                rows.note_loss(at, LossReason::StagingPressure);
            }
        }
    }

    fn mark_action(&self, at: SessionTimeUs, geometry: Evidence<GeometryId>, kind: ProvisionalInputKind) {
        if kind == ProvisionalInputKind::PreMotion { return; }
        if let Evidence::Observed(geometry) = geometry {
            if let Some(endpoint) = self.endpoints.get(&geometry.monitor) {
                endpoint.action(at);
                return;
            }
        }
        // Unknown routing cannot certify quiet on any selected monitor.
        for endpoint in self.endpoints.values() {
            endpoint.action(at);
        }
    }

    pub fn activity(&self, at: SessionTimeUs) {
        for endpoint in self.endpoints.values() {
            endpoint.activity(at);
        }
    }

    pub fn pointer_moved(&mut self, at: SessionTimeUs, admission: AdmissionEpoch, proof: ForegroundAtReceipt,
        focus: Option<FocusGeneration>, pointer: Option<crate::capture::input::PointerEvidence>,
    ) {
        self.activity(at);
        self.motion = focus.zip(pointer).and_then(|(focus, pointer)| {
            let (Evidence::Observed(point), Evidence::Observed(geometry)) = (pointer.event_position, pointer.geometry) else {
                return None;
            };
            Some(MotionRest { point, geometry, proof, focus, admission, last: at })
        });
    }

    fn poll_pointer_rest(&mut self, context: &mut Context<'_>, receipt: Receipt, observed: QpcTick, source: Option<VisitBinding>)
        -> Result<(), ProbeError>
    {
        let now = receipt.clock.time(observed).map_err(clock_error)?;
        let Some(motion) = self.motion.take_if(|motion| now.0.saturating_sub(motion.last.0) >= POINTER_REST_US) else {
            return Ok(());
        };
        if motion.admission != receipt.boundary.epoch || !context.is_current(motion.proof, Some(motion.focus)) {
            return Ok(());
        }
        let Some(endpoint) = self.endpoints.get(&motion.geometry.monitor) else { return Ok(()) };
        let Some(rows) = &self.rows else { return Ok(()) };
        let fallback = stills::fallback_row(stills::Trigger::PointerRest, stills::Role::Image, now,
            crate::protocol::Evidence::Observed(motion.geometry), motion.admission, None, "pointer_rest_handoff_unavailable");
        let reservation = match rows.reserve(now, Some(fallback)) {
            Ok(reservation) => reservation,
            Err(reason) => {
                rows.note_loss(now, reason);
                return Ok(());
            }
        };
        let pointer = context.pointer_query(motion.point, motion.geometry, receipt.clock, observed, None)?;
        endpoint.pointer_rest(stills::Rest { reservation, at: now, admission: motion.admission,
            geometry: motion.geometry, proof: motion.proof, visit: source, pointer });
        Ok(())
    }
    pub fn choose(&self, input: &HeldInput, geometry: Evidence<GeometryId>, proof: ForegroundAtReceipt, kind: ProvisionalInputKind)
        -> Option<ObservationId>
    {
        let correlated = &input.normalized.correlated;
        std::iter::once(&correlated.first).chain(&correlated.partners)
            .filter_map(|slot| slot.observation)
            .filter_map(|id| self.pending.get(&id).map(|entry| (id, entry)))
            .filter(|(_, entry)| entry.envelope.kind == kind)
            .max_by_key(|(_, entry)| (entry.envelope.geometry == geometry && entry.envelope.foreground_at_receipt == Some(proof)
                    && entry.decision.is_some(), entry.envelope.device.is_some(),
                std::cmp::Reverse(entry.envelope.time.recv_t_us)))
            .map(|(id, _)| id)
    }

    pub fn published(
        &mut self, id: Option<ObservationId>, geometry: Evidence<GeometryId>, proof: ForegroundAtReceipt,
        device: Option<DeviceId>, at: SessionTimeUs, kind: ProvisionalInputKind, event: EventRef, admission: AdmissionEpoch,
    ) {
        self.mark_action(at, geometry, kind);
        let Some(mut entry) = id.and_then(|id| self.pending.remove(&id)) else {
            if let Some(rows) = &self.rows {
                for _ in 0..stills::trigger_rows(kind) {
                    rows.note_loss(at, LossReason::RetentionUnavailable);
                }
            }
            return;
        };
        let Some(mut rows) = entry.rows.take() else { return; };
        let routable = match geometry {
            Evidence::Observed(id) if entry.envelope.geometry == geometry && entry.envelope.foreground_at_receipt == Some(proof) => {
                Some(id)
            }
            _ => None,
        };
        let reason = if routable.is_some() { "provisional_retention_unavailable" } else { "monitor_routing_unknown" };
        rows.approve(kind, event, geometry, admission, at, reason);
        if let Some((decision, geometry)) = entry.decision.zip(routable) {
            // A send failure drops the transferred reservations. Approved
            // fallback rows remain attached to those original receipt slots.
            let _ = decision.try_send(stills::Permission { event, geometry, proof, device, at, rows });
        }
    }

    fn stage(&mut self, change: ProvisionalInputChange, source: Option<VisitBinding>) {
        match change {
            ProvisionalInputChange::Opened(request) => {
                // IMG-2: only the first motion after the idle period opens a
                // pre-motion envelope. Later motion stays pending without an
                // envelope or still rows, so its publication reserves nothing.
                let at = request.time.recv_t_us;
                let opens = request.kind != ProvisionalInputKind::PreMotion || {
                    let previous = self.last_motion.replace(self.last_motion.map_or(at, |last| last.max(at)));
                    previous.is_none_or(|last| at.0.saturating_sub(last.0) >= MOTION_IDLE_US)
                };
                let source = source.filter(|_| opens);
                let rows = self.rows.as_ref().filter(|_| opens).map(|lane| stills::EarlyRows::reserve(lane, at, request.kind));
                let decision = if opens && source.is_none() { Self::open(&self.endpoints, request, None) } else { None };
                self.pending.insert(request.id, PrivateRequest { envelope: request, decision, deferred_source: source, rows });
            }
            ProvisionalInputChange::Dropped(id) => self.resolve(id),
        }
    }

    fn open_deferred(&mut self, source: VisitBinding, before: SessionTimeUs) {
        let mut ready: Vec<_> = self.pending.values_mut().filter(|entry| entry.deferred_source == Some(source)
            && entry.envelope.time.recv_t_us < before).collect();
        ready.sort_by_key(|entry| (entry.envelope.time.recv_t_us, entry.envelope.id));
        for entry in ready {
            entry.deferred_source = None;
            entry.decision = Self::open(&self.endpoints, entry.envelope, Some(source));
        }
    }

    fn open(endpoints: &BTreeMap<MonitorId, stills::Requests>, request: ProvisionalInputObservation, visit: Option<VisitBinding>)
        -> Option<SyncSender<stills::Permission>>
    {
        let Evidence::Observed(geometry) = request.geometry else { return None; };
        endpoints.get(&geometry.monitor)?.open(request, visit)
    }

    /// End a provisional request. Its journal outcome is accounted by the
    /// publication owner; the still side only releases the retained request.
    pub fn resolve(&mut self, id: ObservationId) {
        self.pending.remove(&id);
    }

    fn note_gap(&mut self, gap: InputGap) {
        for endpoint in self.endpoints.values() {
            endpoint.action(gap.last);
        }
    }

    /// End the provisional requests of `records`.
    fn resolve_records<'a>(&mut self, records: impl IntoIterator<Item = &'a PendingInput>) {
        for id in records.into_iter().filter_map(|record| record.observation) {
            self.resolve(id);
        }
    }

    pub fn retire_except(&mut self, input: &HeldInput, permit: Option<ObservationId>) {
        let input = &input.normalized.correlated;
        for slot in std::iter::once(&input.first).chain(&input.partners) {
            if let Some(id) = slot.observation.filter(|id| Some(*id) != permit) {
                self.resolve(id);
            }
        }
    }

    pub fn retire_input(&mut self, input: HeldInput) {
        let input = &input.normalized.correlated;
        self.resolve_records(std::iter::once(&input.first).chain(&input.partners));
    }
}
