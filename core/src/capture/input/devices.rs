use std::collections::BTreeMap;

use crate::protocol::{DeviceId, ipc::{DeviceDescriptor, HelperToCore, InputSample, MAX_INPUT_DEVICES}};

const ELEVATED_DEVICE: u64 = 1 << 63;

#[derive(Default)]
pub(crate) struct Devices {
    ordinary: Inventory,
    elevated: Inventory,
    next: u64,
}

#[derive(Default)]
struct Inventory {
    active: BTreeMap<DeviceId, DeviceDescriptor>,
    incomplete: bool,
}

impl Devices {
    pub(crate) fn new_elevated_owner(&mut self) {
        self.elevated = Inventory::default();
    }

    /// The inventory of the elevated or the ordinary owner.
    fn inventory(&self, elevated: bool) -> &Inventory {
        if elevated { &self.elevated } else { &self.ordinary }
    }

    pub(crate) fn current(&self, elevated: bool) -> impl Iterator<Item = &DeviceDescriptor> {
        self.inventory(elevated).active.values()
    }

    pub(crate) fn incomplete(&self, elevated: bool) -> bool {
        self.inventory(elevated).incomplete
    }

    pub(crate) fn translate(&mut self, elevated: bool, mut record: HelperToCore) -> Option<HelperToCore> {
        // Borrowed field by field: an elevated arrival also allocates from `next`.
        let Self { ordinary, elevated: elevated_inventory, next } = self;
        let inventory = if elevated { elevated_inventory } else { ordinary };
        match &mut record {
            HelperToCore::DeviceArrived { descriptor, .. } => {
                let native = descriptor.id;
                descriptor.id = if let Some(existing) = inventory.active.get(&native) {
                    existing.id
                } else {
                    if inventory.active.len() == MAX_INPUT_DEVICES || native.0 >= ELEVATED_DEVICE {
                        inventory.incomplete = true;
                        return None;
                    }
                    if elevated {
                        if *next >= ELEVATED_DEVICE - 1 {
                            inventory.incomplete = true;
                            return None;
                        }
                        *next += 1;
                        DeviceId(ELEVATED_DEVICE | *next)
                    } else {
                        native
                    }
                };
                inventory.active.insert(native, descriptor.clone());
            }
            HelperToCore::DeviceRemoved { id, .. } => {
                *id = inventory.active.remove(id)?.id;
            }
            HelperToCore::Input(input) => match &mut input.sample {
                InputSample::RawKey(input) => translate_optional(inventory, &mut input.device),
                InputSample::RawPointer(input) => translate_optional(inventory, &mut input.device),
                InputSample::Hid(report) => {
                    report.device = match inventory.active.get(&report.device) {
                        Some(device) => device.id,
                        None => {
                            inventory.incomplete = true;
                            return None;
                        }
                    };
                }
                _ => {}
            },
            HelperToCore::Gap(gap) if gap.producer == crate::protocol::ipc::Producer::DeviceNotifications => {
                inventory.incomplete = true;
            }
            _ => {}
        }
        Some(record)
    }
}

fn translate_optional(inventory: &mut Inventory, id: &mut Option<DeviceId>) {
    if let Some(native) = *id {
        *id = inventory.active.get(&native).map(|device| device.id);
        inventory.incomplete |= id.is_none();
    }
}
