//! Per-PTY kitty keyboard mode tracking.

const MAX_CARRY_BYTES: usize = 64;

/// Observes kitty keyboard mode escape sequences emitted by one child PTY.
#[derive(Debug, Default)]
pub struct KittyKeyboardTracker {
    base: u8,
    stack: Vec<u8>,
    carry: Vec<u8>,
}

impl KittyKeyboardTracker {
    /// Observe child output without forwarding or answering any terminal sequence.
    pub fn observe(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);

        let mut index = 0;
        while index < data.len() {
            if data[index] != 0x1b {
                index += 1;
                continue;
            }
            if index + 1 >= data.len() {
                break;
            }
            if data[index + 1] != b'[' {
                index += 1;
                continue;
            }
            if index + 2 >= data.len() {
                break;
            }

            let operation = data[index + 2];
            if !matches!(operation, b'>' | b'<' | b'=') {
                index += 1;
                continue;
            }

            let digits_start = index + 3;
            let mut end = digits_start;
            while end < data.len() && data[end].is_ascii_digit() {
                end += 1;
            }
            if end == data.len() {
                break;
            }
            if data[end] != b'u' {
                index += 1;
                continue;
            }

            let value = std::str::from_utf8(&data[digits_start..end])
                .ok()
                .and_then(|digits| (!digits.is_empty()).then_some(digits))
                .and_then(|digits| digits.parse::<u8>().ok());
            self.apply(operation, value);
            index = end + 1;
        }

        if index < data.len() && data[index..].len() <= MAX_CARRY_BYTES {
            self.carry.extend_from_slice(&data[index..]);
        }
    }

    pub fn active(&self) -> bool {
        self.stack.last().copied().unwrap_or(self.base) != 0
    }

    fn apply(&mut self, operation: u8, value: Option<u8>) {
        match operation {
            b'>' => self.stack.push(value.unwrap_or(0)),
            b'<' => {
                for _ in 0..usize::from(value.unwrap_or(1)) {
                    if self.stack.pop().is_none() {
                        break;
                    }
                }
            }
            b'=' => {
                let flags = value.unwrap_or(0);
                if let Some(current) = self.stack.last_mut() {
                    *current = flags;
                } else {
                    self.base = flags;
                }
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KittyKeyboardTracker;

    #[test]
    fn push_enables_kitty_keyboard() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[>1u");
        assert!(tracker.active());
    }

    #[test]
    fn pop_disables_kitty_keyboard() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[>1u\x1b[<1u");
        assert!(!tracker.active());
    }

    #[test]
    fn set_zero_disables_kitty_keyboard() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[=1u\x1b[=0u");
        assert!(!tracker.active());
    }

    #[test]
    fn empty_set_disables_kitty_keyboard() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[=1u\x1b[=u");
        assert!(!tracker.active());
    }

    #[test]
    fn nested_push_and_pop_restore_the_previous_flags() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[>1u\x1b[>0u");
        assert!(!tracker.active());
        tracker.observe(b"\x1b[<u");
        assert!(tracker.active());
        tracker.observe(b"\x1b[<u");
        assert!(!tracker.active());
    }

    #[test]
    fn split_sequence_is_observed() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[>1");
        assert!(!tracker.active());
        tracker.observe(b"u");
        assert!(tracker.active());
    }

    #[test]
    fn irrelevant_ansi_does_not_change_state() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[31mred\x1b[?1h\x1b[>not-u");
        assert!(!tracker.active());
    }
}
