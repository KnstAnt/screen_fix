//! Small generic helpers shared across the recorder.

/// Lowers `slot` to `value` when `value` is earlier, or sets an empty slot.
pub(crate) fn keep_earlier<T: Ord>(slot: &mut Option<T>, value: T) {
    *slot = Some(match slot.take() { Some(old) => old.min(value), None => value });
}

/// Raises `slot` to `value` when `value` is later, or sets an empty slot.
pub(crate) fn raise<T: Ord>(slot: &mut Option<T>, value: T) {
    *slot = Some(match slot.take() { Some(old) => old.max(value), None => value });
}

/// Widens the `(first, last)` span in `slot` to cover `at`, or starts it at `at`.
pub(crate) fn widen<T: Ord + Copy>(slot: &mut Option<(T, T)>, at: T) {
    *slot = Some(slot.map_or((at, at), |(first, last)| (first.min(at), last.max(at))));
}

/// Appends `item` while `items` holds fewer than `max`; `false` when it was dropped.
pub(crate) fn push_bounded<T>(items: &mut Vec<T>, item: T, max: usize) -> bool {
    let room = items.len() < max;
    if room {
        items.push(item);
    }
    room
}

/// Whether two items of `items` are `same`.
pub(crate) fn has_duplicate<T>(items: &[T], same: impl Fn(&T, &T) -> bool) -> bool {
    items.iter().enumerate().any(|(index, item)| items[..index].iter().any(|previous| same(previous, item)))
}
