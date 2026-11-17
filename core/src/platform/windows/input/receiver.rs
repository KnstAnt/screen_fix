use std::{collections::BTreeMap, sync::mpsc::SyncSender};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{HKL, MAPVK_VK_TO_CHAR, MAPVK_VK_TO_VSC_EX, MapVirtualKeyExW};

use crate::{
    capture::{
        DropReason,
        input::{
            ControlChords, HeldBoundary, HeldInput, InputGap,
            InputMatcher, InputResolution, InputStep, Key, MotionSampler, NativeLoss,
            NormalizedInput, PhysicalHeld, ProvisionalInputChange, Sampled, ScanCode, ScanPrefix, SyntheticHeldEdge,
            modifier_bit, modifier_of, producer_index, startup_modifier,
        },
        observation::{ProvisionalInputKind, ProvisionalInputObservation},
    },
    package::{journal::JournalIngress, metadata::UncertifiedInputTail},
    protocol::{
        AdmissionEpoch, BoundedList, DeviceId, Evidence, EventRef, FocusGeneration, GeometryId, MonitorId, ObservationId, PhysicalPoint, QpcTick, SessionTimeUs, UnavailableReason,
        ipc::{DeviceDescriptor, ForegroundAtReceipt, HelperToCore, InputSample, MAX_INPUT_DEVICES, Producer, elevated::{InputEvidence, VisitBinding}},
    },
    session::{AdmissionBoundary, hotkey::ControlOutcome, windows::{ControlPrefix, Receipt}},
};

use super::{ProbeError, ReceiptPrefix, clock, clock_error, devices::Devices, gate::{Gate, Selection}, hook_health::HookHealth, input_timing, resync, unavailable};
use super::super::{context::{ConfirmedText, Context, Visit}, elevated::InputCertificate, still_rows, stills};

const STALLED_PROGRESS_US: i64 = 250_000;
const SYNC_RETRY_US: i64 = 100_000;

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
}

impl Receiver {
    pub fn new(hotkey_keys: [u16; 4], endpoints: BTreeMap<MonitorId, stills::Requests>, rows: still_rows::Rows) -> Self {
        Self {
            matcher: InputMatcher::new(), provisional: ProvisionalRequests { endpoints, rows: Some(rows), ..ProvisionalRequests::default() }, overflow: false,
            physical: PhysicalHeld::default(), gate: Gate::new(), sampler: MotionSampler::default(),
            controls: ControlChords::new(hotkey_keys),
            devices: BTreeMap::new(), device_inventory_truncated: false,
            hook_health: HookHealth::default(), opened_at: None, sync_after: None, last_sync_attempt: None,
            requires_opening_sync: true, synchronized_qpc: None, sync_visit: None,
            source: None, source_certificate: None, device_ids: Devices::default(), past_hook_loss: (false, false),
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
        let record = self.device_ids.translate(elevated, record);
        if self.device_ids.incomplete(elevated) {
            self.device_inventory_truncated = true;
            self.gate.note_device_inventory_loss();
        }
        let Some(record) = record else { return Ok(()); };
        match record {
            HelperToCore::Input(packet) => {
                if closing.is_some_and(|closing| packet.receipt.qpc >= closing.qpc) {
                    return Ok(());
                }
                if !elevated && packet.foreground_at_receipt.is_some_and(|proof| !context.ordinary_process(proof.process)) {
                    return Ok(());
                }
                let Some(timing) = input_timing(receipt, packet.receipt.qpc, packet.receipt.message_tick_ms)? else {
                    if packet.receipt.qpc < receipt.boundary.qpc && let Some(modifier) = startup_modifier(&packet) {
                        self.controls.note_startup_change(modifier);
                    }
                    return Ok(());
                };
                self.hook_health.note_callback(packet.sample.producer(), timing.time.recv_t_us);
                if self.requires_opening_sync || self.synchronized_qpc.is_some_and(|after| packet.receipt.qpc <= after) {
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
            }
            HelperToCore::Gap(gap) => {
                self.inventory(HelperToCore::Gap(gap));
                if gap.last_qpc < receipt.boundary.qpc || closing.is_some_and(|end| gap.first_qpc >= end.qpc) {
                    return Ok(());
                }
                let first = receipt.clock.time(gap.first_qpc.max(receipt.boundary.qpc)).map_err(clock_error)?;
                let last = receipt.clock.time(closing.map_or(gap.last_qpc, |end| gap.last_qpc.min(end.qpc))).map_err(clock_error)?;
                let clipped = gap.first_qpc < receipt.boundary.qpc || closing.is_some_and(|end| gap.last_qpc > end.qpc);
                self.matcher.gap(InputGap {
                    producer: gap.producer, first, last, logical_slots: 0,
                    native: Some(NativeLoss {
                        first_sequence: gap.first_sequence, last_sequence: gap.last_sequence,
                        admitted_count: (!clipped).then(|| gap.last_sequence.0.saturating_sub(gap.first_sequence.0).saturating_add(1)),
                    }),
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

    pub fn countdown(&mut self, record: HelperToCore) {
        if let HelperToCore::Input(input) = &record && let Some(modifier) = startup_modifier(input) {
            self.controls.note_startup_change(modifier);
        }
        self.collect_ordinary_inventory(record, true);
    }

    pub fn collect_ordinary_inventory(&mut self, record: HelperToCore, selected: bool) {
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
        for descriptor in self.device_ids.current(source.is_some()) {
            self.gate.note_input_device(descriptor);
            if self.devices.len() < MAX_INPUT_DEVICES || self.devices.contains_key(&descriptor.id) {
                self.devices.insert(descriptor.id, descriptor.clone());
            } else {
                self.device_inventory_truncated = true;
            }
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
            HelperToCore::DeviceArrived { descriptor, .. } => {
                if self.devices.len() < MAX_INPUT_DEVICES || self.devices.contains_key(&descriptor.id) {
                    self.gate.note_input_device(&descriptor);
                    self.devices.insert(descriptor.id, descriptor);
                } else {
                    self.device_inventory_truncated = true;
                    self.gate.note_device_inventory_loss();
                }
            }
            HelperToCore::DeviceRemoved { id, .. } => self.gate.forget_input_device(id),
            HelperToCore::Gap(gap) if gap.producer == crate::protocol::ipc::Producer::DeviceNotifications => {
                self.device_inventory_truncated = true;
                self.gate.note_device_inventory_loss();
            }
            _ => {}
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

    fn install_synchronization(
        &mut self, qpc: QpcTick, at: SessionTimeUs, state: crate::capture::input::HeldSynchronization, visit: Option<Visit>,
    ) {
        self.sync_visit = visit;
        self.matcher.stage_synchronization(qpc, at, state);
        self.requires_opening_sync = false;
        self.synchronized_qpc = Some(qpc);
        self.sync_after = None;
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

    pub fn publish(
        &mut self, context: &mut Context<'_>, output: &mut JournalIngress, prefix: &mut ReceiptPrefix,
        receipt: Receipt, through: SessionTimeUs, final_boundary: bool,
    ) -> Result<(), ProbeError> {
        let certified = self.matched_and_sampled_before();
        let certified = if let Some(before) = self.elevated_hook_loss() {
            let at = receipt.clock.time(before).map_err(clock_error)?;
            certified.map(|bound| bound.min(at))
        } else { certified };
        self.gate.drain(context, output, prefix, &mut self.provisional, receipt, through, certified, final_boundary)
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
        self.hook_health.reset_streaks();
        self.provisional.motion = None;
        self.controls.reopen(hotkey);
    }

    pub fn interrupt(&mut self, cutoff: SessionTimeUs, boundary: HeldBoundary) {
        self.interrupt_inner(cutoff, boundary, matches!(boundary, HeldBoundary::Pause | HeldBoundary::Stop).then_some(cutoff));
    }

    pub fn interrupt_certified(&mut self, cutoff: SessionTimeUs, close_at: SessionTimeUs, boundary: HeldBoundary) {
        self.interrupt_inner(close_at, boundary, Some(cutoff));
    }

    fn interrupt_inner(&mut self, close_at: SessionTimeUs, boundary: HeldBoundary, certificate: Option<SessionTimeUs>) {
        let cutoff = certificate.unwrap_or(close_at);
        if self.matcher.certified_before().is_none_or(|bound| bound <= cutoff) {
            self.controls.note_loss(cutoff);
        }
        let bound = if certificate.is_some() {
            self.matcher.certified_before().map(|before| SessionTimeUs(before.min(cutoff).0.saturating_sub(1)))
        } else {
            Some(cutoff)
        };
        if let Some(bound) = bound {
            while self.elevated_hook_loss().is_none()
                && let Some(step) = self.matcher.finish(bound)
            { self.consume(step); }
        }
        self.process_samples(|sampler, emit| sampler.finish(emit));
        self.release_controls(Some(cutoff), cutoff, true);
        self.controls.close_interval(cutoff);
        self.physical.invalidate();
        let hook_before = self.elevated_hook_loss();
        if let Some(before) = certificate {
            self.gate.retain_prefix(before, hook_before, &mut self.provisional);
        }
        if hook_before.is_some() {
            self.gate.boundary(close_at, HeldBoundary::HookUnavailable, Selection::All);
        }
        self.gate.boundary(close_at, boundary, Selection::All);
        for input in self.matcher.discard() {
            if let Some(observation) = input.observation {
                self.provisional.resolve(observation, InputResolution::Dropped(DropReason::AdmissionClosed));
            }
        }
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

    pub fn stalled_producer(&self, at: SessionTimeUs) -> Option<Producer> {
        let opened = self.opened_at?;
        [Producer::RawInput, Producer::DeviceNotifications, Producer::KeyboardHook, Producer::PointerHook]
            .into_iter().find(|producer| {
                let most_recent = self.matcher.producer_bound(*producer).unwrap_or(opened).max(opened);
                at.0.saturating_sub(most_recent.0) > STALLED_PROGRESS_US
            })
    }

    fn consume(&mut self, step: InputStep) {
        match step {
            InputStep::Record(input) => {
                // Ordinary handling keeps its original HID and hook boundary order.
                let elevated = self.source.is_some();
                if elevated {
                    self.hook_health.note_correlation(&input);
                    if self.elevated_hook_loss().is_some() {
                        for receipt in std::iter::once(&input.first).chain(&input.partners) {
                            if let Some(id) = receipt.observation {
                                self.provisional.resolve(id, InputResolution::Dropped(DropReason::AdmissionClosed));
                            }
                        }
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
                self.process_samples(|sampler, emit| sampler.push(held, pointer_lost, emit));
            }
            InputStep::Gap(gap) => {
                self.overflow = true;
                self.hook_health.reset_streaks();
                self.flush_motion();
                self.apply_gap(gap.first, gap.last, HeldBoundary::InputGap, [Some(gap)]);
            }
            InputStep::DeviceRemoved { id, at } => {
                self.flush_motion();
                self.controls.note_loss(at);
                self.release_controls(Some(at), at, true);
                self.controls.close_interval(at);
                self.physical.remove(id);
                self.gate.boundary(at, HeldBoundary::DeviceRemoved, Selection::Removed(id));
                self.request_sync(at);
            }
            InputStep::Synchronize { state, at } => {
                if self.flush_motion() {
                    self.request_sync(at);
                    return;
                }
                self.release_controls(Some(at), at, true);
                let omitted = self.controls.omitted_from_sync(state);
                let mut edges = self.physical.synchronize(state, at);
                edges.retain(|edge| modifier_of(edge.edge.identity).is_none_or(|modifier| omitted & modifier_bit(modifier) == 0));
                if let Some(visit) = self.sync_visit.take() {
                    let eligible_modifiers = !omitted;
                    self.gate.synchronize(at, visit,
                        state.modifiers & eligible_modifiers, !state.modifiers & eligible_modifiers, edges);
                } else {
                    self.provisional.suppress_synthetic(edges);
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

#[derive(Default)]
struct SampleLoss {
    interval: Option<(SessionTimeUs, SessionTimeUs)>,
    by_producer: [Option<InputGap>; 4],
}

impl SampleLoss {
    fn record(&mut self, input: HeldInput, requests: &mut ProvisionalRequests) {
        let slot = &input.normalized.correlated.first;
        let at = slot.time.recv_t_us;
        let producer = slot.packet.sample.producer();
        let gap = InputGap { producer, first: at, last: at, native: None, logical_slots: 1 };
        let pending = &mut self.by_producer[producer_index(producer)];
        *pending = Some(pending.map_or(gap, |old| old.merge(gap)));
        self.interval = Some(self.interval.map_or((at, at), |(first, last)| (first.min(at), last.max(at))));
        requests.retire_input(input, InputResolution::Dropped(DropReason::Capacity));
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
    suppressed: u64,
    dropped: u64,
    synthetic_suppressed: u64,
    private_gaps: [Option<InputGap>; 4],
}

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

    pub fn approved_observation_gap(&self, at: SessionTimeUs, geometry: Evidence<GeometryId>, kind: Option<ProvisionalInputKind>) {
        if let Some(kind) = kind {
            self.mark_action(at, geometry, kind);
        }
        if let Some(rows) = &self.rows {
            rows.note_loss(at);
            rows.note_loss(at);
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
        let Some(motion) = &self.motion else { return Ok(()) };
        if now.0.saturating_sub(motion.last.0) < 500_000 { return Ok(()); }
        let motion = self.motion.take().expect("the pointer rest timer was present");
        if motion.admission != receipt.boundary.epoch || !context.is_current(motion.proof, Some(motion.focus)) {
            return Ok(());
        }
        let Some(endpoint) = self.endpoints.get(&motion.geometry.monitor) else { return Ok(()) };
        let Some(rows) = &self.rows else { return Ok(()) };
        let fallback = stills::dropped_trigger("pointer_rest", "image", now, motion.geometry, motion.admission, "pointer_rest_handoff_unavailable");
        let Some(reservation) = rows.reserve(now, Some(fallback)) else {
            rows.note_loss(now);
            return Ok(());
        };
        let pointer = context.pointer_query(motion.point, motion.geometry, receipt.clock, observed, None)?;
        endpoint.pointer_rest(stills::Rest { reservation, at: now, admission: motion.admission,
            geometry: motion.geometry, proof: motion.proof, visit: source, pointer });
        Ok(())
    }
    pub fn choose(&self, input: &crate::capture::input::HeldInput, geometry: Evidence<GeometryId>, proof: ForegroundAtReceipt, kind: ProvisionalInputKind)
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
                rows.note_loss(at);
                rows.note_loss(at);
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
                let rows = self.rows.as_ref().map(|lane| stills::EarlyRows::reserve(lane, request.time.recv_t_us));
                let decision = if source.is_none() { Self::open(&self.endpoints, request, None) } else { None };
                self.pending.insert(request.id, PrivateRequest { envelope: request, decision, deferred_source: source, rows });
            }
            ProvisionalInputChange::Dropped(id, reason) => self.resolve(id, InputResolution::Dropped(reason)),
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

    pub fn resolve(&mut self, id: ObservationId, resolution: InputResolution) {
        if self.pending.remove(&id).is_none() { return; }
        match resolution {
            InputResolution::Suppressed => self.suppressed = self.suppressed.saturating_add(1),
            InputResolution::Dropped(_) => self.dropped = self.dropped.saturating_add(1),
            InputResolution::Published(_) => {}
        }
    }

    pub(super) fn suppress_synthetic(&mut self, edges: Vec<SyntheticHeldEdge>) {
        self.synthetic_suppressed = self.synthetic_suppressed.saturating_add(edges.len() as u64);
    }

    fn note_gap(&mut self, gap: InputGap) {
        for endpoint in self.endpoints.values() {
            endpoint.action(gap.last);
        }
        let private = &mut self.private_gaps[producer_index(gap.producer)];
        *private = Some(private.map_or(gap, |old| old.merge(gap)));
    }

    pub fn retire_except(&mut self, input: &crate::capture::input::HeldInput, permit: Option<ObservationId>) {
        let input = &input.normalized.correlated;
        for slot in std::iter::once(&input.first).chain(&input.partners) {
            if let Some(id) = slot.observation.filter(|id| Some(*id) != permit) {
                self.resolve(id, InputResolution::Suppressed);
            }
        }
    }

    pub fn retire_input(&mut self, input: crate::capture::input::HeldInput, resolution: InputResolution) {
        let input = input.normalized.correlated;
        for slot in std::iter::once(input.first).chain(input.partners) {
            if let Some(observation) = slot.observation {
                self.resolve(observation, resolution);
            }
        }
    }
}
